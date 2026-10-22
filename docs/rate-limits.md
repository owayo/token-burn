# Rate limits

How token-burn holds back new tasks when an account's usage window fills up, and how it classifies tasks that ended on a limit. The threshold is `rate_limit_threshold` in [Settings](configuration.md#settings).

## Checking the threshold

`rate_limit_threshold` is enforced on two paths. During a task, Claude Code's stream-json `rate_limit_event` is monitored in real time and new tasks are held back once the threshold is exceeded. In addition, when [ai-usage integration](configuration.md#ai-usage-integration-optional) is enabled, after each task completes the agent's real utilization is re-checked against this threshold using the matching `(profile, provider)` pair's `weekly` and `five_hour` `used_percent` values; this applies to both `claude` and `codex` agents, so `codex`, which has no real-time monitoring, stops too.

Both paths decide **how** to hold back by the period of the window that crossed the threshold. A weekly window shares its period with the run's deadline, so it stops the run permanently. A short window — `five_hour`, or a 24-hour window reported as `kind:"daily"` — only pauses until that window resets; a worker rechecks usage before taking another task. The period comes from `kind`, never from the slot name, because providers do report a 24-hour window in the `five_hour` slot. A pause falls back to a permanent stop when the reset time is missing or implausible (further ahead than one period of that window). It also stops when the reset comes after the run's deadline, but with ai-usage integration the workers wait until the deadline instead, in case the window is reset manually.

With ai-usage integration, these checks use usage fetched with `--refresh`, bypassing ai-usage's shared cache (120 seconds by default): a cached value can still show the pre-reset usage right after a manual reset. Only the running account is fetched (`--profile` and `--only`), so the other accounts are not queried. An older ai-usage that does not know `--refresh` falls back to a plain fetch, and the early resume below does not happen.

## Resuming early after a manual reset

Claude ("Reset for free" in Settings > Usage on claude.ai) and Codex (banked resets) let you refill a usage window at any time. A pause's resume time is only an estimate made from the observation that stopped the run. With ai-usage integration, token-burn looks the reset up from live data every time instead of trusting the recorded time, and resumes as soon as a manual reset refills the window.

While a pause is in effect, the monitor refetches the running account's usage about every 30 seconds. Once the window that caused the pause has dropped to half of its level at the pause (and by at least 10 points) and every window is under the threshold, the monitor lifts the pause. The waiting workers notice within a second, recheck usage, and take their next task. A small drop does not lift a pause: the API and the usage page on the web round and lag differently, and requests can be rejected for reasons other than the usage percentage.

The start of a run is decided from live data too. If the five-hour window is at or above the threshold, the run starts paused until that window resets; if the weekly window is, no task starts. No task starts either when the usage cannot be confirmed (ai-usage fails, or it reports an error for the account). A session interrupted by a five-hour rejection in an earlier run no longer holds the next run back once the window has been refilled.

If the API rejects requests again right after an early resume, that pause is not lifted by comparing ai-usage values with API values. When ai-usage is looking at a different account from the one actually running, lifting and getting rejected would repeat until the queue is used up by failed tasks.

Without ai-usage integration there is no way to look the usage up, so workers wait until the estimated time.

## Stopping and pausing

Workers reread the pause file every second while waiting. If another worker extends the pause, the remaining time, window, and stop reason follow the new observation. An extension to or past the deadline stops the run without waiting for the original resume time. An unreadable or corrupt pause file also stops the worker. The monitor updates its resume-time notice when the pause changes.

- **A spent five-hour window does not throw away the whole run**: Stops are split by how long the window takes to reset. A weekly window (the same period as the run's deadline) means a permanent stop, but a window that recovers on its own — the five-hour window — only pauses until that window resets, and workers continue with the remaining tasks once the time passes. A permanent stop there would discard every task left before the weekly reset: in a real log the five-hour window reset minutes after touching the threshold, with the weekly window at 43% and 4h52m left before the deadline. Waiting past the deadline, or a reset time that cannot be read, stops permanently. Resuming from a pause rechecks real utilization; if that check extends the pause, the worker checks again after the new resume time. A failed check stops that worker even if it cannot write the stop file
- **Checking for a stop and taking a task are inseparable**: A worker does both under one lock. While they were two steps, a stop published in between started one more task after the run was supposed to have stopped
- **Usage-rate gate**: When ai-usage integration is enabled, re-checks each agent's real utilization (`weekly` / `five_hour`) per window after every task and stops or pauses new tasks once `rate_limit_threshold` is reached — extending threshold-based auto-stop to `codex`, not just Claude Code's in-task `rate_limit_event`
- **Auto-stop keyed to the windows that actually gate execution**: The stop decision uses the `five_hour` / `seven_day` utilization from `unifiedWindows`, never the monthly overage window. Real logs carry `rateLimitType:"overage"` / `utilization:1.03` warnings while the five-hour window sits at 13%, and comparing that number against the threshold used to stop every task. The stop line now names the window it acted on, prints **that window's own** reset time, and appends the measured values as `[5h 13% / 7d 54%]`; overage warnings are shown as `(overage, no auto-stop)` without stopping anything
- **Rate limit alerts**: Displays utilization warnings, rejected request notifications, overage status and overage reset time (shown on warning and rejection events too), and the server-side warning threshold that was crossed (e.g. `warning at 90%`) for `allowed_warning` events; holds back further tasks when the configured threshold is exceeded — permanently for the seven-day window, until the window resets for the five-hour one (if the stop file cannot be created due to ENOSPC/permissions, the failure is surfaced instead of being silently swallowed)

## Classifying limited tasks

- **Limit-aware result classification**: Treats limit-reached results — clock times including minutes such as `resets 2:30am`, and messages such as `You've hit your session limit` or `You've hit your org's monthly spend limit` — as rate limits rather than retryable provider errors, since retrying cannot clear them
- **Dated reset times**: Reset times that fall on a later day are shown as `MM/DD HH:MM`, since `seven_day` and overage windows can reset up to a month out and a bare clock time reads as "later today"
- **Transient connection errors retried**: Connection-level failures without an HTTP status (e.g. `API Error: Connection closed mid-response`) are classified as retryable rather than permanent, so the worker moves on to the next target instead of stopping (the target is reprocessed on the next run)

A Claude Code task cut off by a rate limit can continue on the next run; see [Resuming interrupted sessions](usage.md#resuming-interrupted-sessions).
