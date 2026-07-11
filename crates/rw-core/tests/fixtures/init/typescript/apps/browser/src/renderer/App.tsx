import { useEffect, useRef, useState } from 'react'
import type { BrowserState } from '@shared/types'

// NOTE: this is a deliberately plain test harness to drive and verify the
// engine. It is NOT the PaperDesign chrome - that replaces this later, against
// the exact same window.sora contract, with zero engine changes.

export function App(): JSX.Element {
  const [state, setState] = useState<BrowserState | null>(null)
  const [addr, setAddr] = useState('')
  const barRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const off = window.sora.onState(setState)
    window.sora.requestState()
    return off
  }, [])

  // Report the chrome's content-region insets so the engine can size tab views.
  useEffect(() => {
    const report = (): void => {
      const top = barRef.current?.getBoundingClientRect().height ?? 96
      window.sora.layout.reportInsets({ top: Math.round(top), left: 0, right: 0, bottom: 0 })
    }
    report()
    const ro = new ResizeObserver(report)
    if (barRef.current) ro.observe(barRef.current)
    window.addEventListener('resize', report)
    return () => {
      ro.disconnect()
      window.removeEventListener('resize', report)
    }
  }, [state?.tabs.length])

  const active = state?.tabs.find((t) => t.id === state.activeTabId) ?? null

  useEffect(() => {
    setAddr(active && active.url !== 'about:blank' ? active.url : '')
  }, [active?.id, active?.url])

  if (!state) return <div className="boot">booting Sora base…</div>

  const spaceTabs = state.tabs.filter((t) => t.spaceId === state.activeSpaceId)
  const go = (): void => {
    if (active) window.sora.tabs.navigate(active.id, addr)
  }

  return (
    <div className="app">
      <div className="titlebar">
        <span className="brand">
          Sora <small>base harness</small>
        </span>
        <div className="wincontrols">
          <button onClick={() => window.sora.window.minimize()}>—</button>
          <button onClick={() => window.sora.window.maximize()}>▢</button>
          <button className="close" onClick={() => window.sora.window.close()}>
            ✕
          </button>
        </div>
      </div>

      <div ref={barRef} className="chromebar">
        <div className="spaces">
          {state.spaces.map((s) => (
            <button
              key={s.id}
              className={'space' + (s.id === state.activeSpaceId ? ' on' : '')}
              style={{ '--dot': s.color } as React.CSSProperties}
              onClick={() => window.sora.spaces.activate(s.id)}
            >
              <i className="dot" /> {s.name}
            </button>
          ))}
        </div>

        <div className="nav">
          <button disabled={!active?.canGoBack} onClick={() => active && window.sora.tabs.back(active.id)}>
            ◀
          </button>
          <button
            disabled={!active?.canGoForward}
            onClick={() => active && window.sora.tabs.forward(active.id)}
          >
            ▶
          </button>
          <button
            onClick={() =>
              active && (active.loading ? window.sora.tabs.stop(active.id) : window.sora.tabs.reload(active.id))
            }
          >
            {active?.loading ? '✕' : '⟳'}
          </button>
          <input
            className="addr"
            value={addr}
            placeholder="Search or enter address"
            onChange={(e) => setAddr(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') go()
            }}
          />
          <button
            onClick={() =>
              window.sora.layout.set(
                state.layout.mode === 'split' ? 'single' : 'split',
                spaceTabs.slice(0, 2).map((t) => t.id)
              )
            }
          >
            {state.layout.mode === 'split' ? 'unsplit' : 'split'}
          </button>
        </div>

        <div className="tabstrip">
          {spaceTabs.map((t) => (
            <div
              key={t.id}
              className={'tab' + (t.id === state.activeTabId ? ' on' : '')}
              onClick={() => window.sora.tabs.activate(t.id)}
            >
              {t.favicon ? <img src={t.favicon} alt="" /> : <i className="fav" />}
              <span>{t.loading ? '…' : t.title || 'New Tab'}</span>
              <button
                className="x"
                onClick={(e) => {
                  e.stopPropagation()
                  window.sora.tabs.close(t.id)
                }}
              >
                ✕
              </button>
            </div>
          ))}
          <button className="newtab" onClick={() => window.sora.tabs.create({})}>
            +
          </button>
        </div>
      </div>
      {/* content region intentionally empty: engine-owned tab views render here */}
    </div>
  )
}
