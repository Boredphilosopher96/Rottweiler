mod device_flow;
mod models;
mod provider;

pub use device_flow::{
    DeviceFlowCancellation, GITHUB_COPILOT_ACCESS_TOKEN_ENDPOINT, GITHUB_COPILOT_CLIENT_ID,
    GITHUB_COPILOT_DEVICE_CODE_ENDPOINT, GitHubCopilotAccessToken,
    GitHubCopilotDeviceAuthorization, GitHubCopilotDeviceFlow, GitHubCopilotDeviceSession,
    GitHubDeviceFlowTransport, GitHubDevicePoll,
};
pub use models::{
    GitHubCopilotCatalog, GitHubCopilotEndpoint, GitHubCopilotModel, GitHubCopilotPricing,
    github_copilot_ai_credits, github_copilot_micros_usd_per_million, parse_github_copilot_models,
};
pub use provider::{
    GITHUB_COPILOT_API_VERSION, GITHUB_COPILOT_BASE_URL, GitHubCopilotProvider,
    GitHubCopilotProviderConfig, GitHubCopilotRuntime,
};

pub(crate) use provider::replay_sse_frames;
