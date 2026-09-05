use super::*;

struct RejectingWorktreeFactory {
    change: bool,
    allocated: Arc<Mutex<Option<PathBuf>>>,
}
#[async_trait]
impl SubagentSessionFactory for RejectingWorktreeFactory {
    async fn create(
        &self,
        launch: SubagentLaunch,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
        *self.allocated.lock().expect("allocated path") = Some(launch.workspace_root.clone());
        if self.change {
            std::fs::write(launch.workspace_root.join("keep.txt"), "partial child work")
                .expect("write before rejection");
        }
        Err(OrchestrationError::Session(
            "child configuration rejected".to_owned(),
        ))
    }
}

fn git_repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("repository");
    std::fs::write(directory.path().join("tracked.txt"), "base").expect("base file");
    for args in [
        ["init", "--quiet"].as_slice(),
        ["add", "tracked.txt"].as_slice(),
        ["commit", "--quiet", "-m", "base"].as_slice(),
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(directory.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("git fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    directory
}

#[tokio::test]
async fn rejected_child_configuration_proves_worktree_cleanup_before_releasing_admission() {
    for change in [false, true] {
        let repository = git_repository();
        let private = tempfile::tempdir().expect("private root");
        let isolation = Arc::new(
            WorktreeIsolation::new(
                repository.path(),
                private.path(),
                rw_tools::WorktreeLimits::default(),
                CancellationToken::default(),
            )
            .await
            .expect("isolation"),
        );
        let allocated = Arc::new(Mutex::new(None));
        let factory = Arc::new(WorktreeSubagentSessionFactory::new(
            Arc::new(RejectingWorktreeFactory {
                change,
                allocated: Arc::clone(&allocated),
            }),
            isolation,
        ));
        let orchestrator = SubagentOrchestrator::new(
            SubagentLimits {
                max_concurrency: 1,
                ..SubagentLimits::default()
            },
            factory,
            Arc::new(ToolRegistry::new()),
        )
        .expect("orchestrator");
        let mut request = request("rejected");
        request.isolation = SubagentIsolation::Worktree;
        request.workspace_root = repository.path().to_path_buf();
        let error = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request.clone(),
                Arc::new(RecordingObserver::default()),
                CancellationToken::default(),
            )
            .await
            .expect_err("configuration rejected");
        let path = allocated
            .lock()
            .expect("allocated path")
            .clone()
            .expect("created worktree");
        if change {
            assert!(matches!(error, OrchestrationError::EffectsUnsettled(_)));
            assert_eq!(
                std::fs::read_to_string(path.join("keep.txt")).expect("retained work"),
                "partial child work"
            );
            assert!(orchestrator.settle_startups().await.is_err());
            assert!(matches!(
                orchestrator
                    .start(
                        SessionId("parent".to_owned()),
                        request,
                        Arc::new(RecordingObserver::default()),
                        CancellationToken::default()
                    )
                    .await,
                Err(OrchestrationError::ConcurrencyExceeded { .. })
            ));
        } else {
            assert!(matches!(error, OrchestrationError::Session(_)));
            assert!(!path.exists());
            orchestrator.settle_startups().await.expect("cleanup proof");
            assert!(matches!(
                orchestrator
                    .start(
                        SessionId("parent".to_owned()),
                        request,
                        Arc::new(RecordingObserver::default()),
                        CancellationToken::default()
                    )
                    .await,
                Err(OrchestrationError::Session(_))
            ));
            orchestrator
                .settle_startups()
                .await
                .expect("second cleanup proof");
            assert!(
                !allocated
                    .lock()
                    .expect("allocated path")
                    .as_ref()
                    .expect("second allocation")
                    .exists()
            );
        }
    }
}
