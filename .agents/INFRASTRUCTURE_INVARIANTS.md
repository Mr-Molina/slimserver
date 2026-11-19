# Infrastructure Invariants & Mechanical Guardrails

The following mechanical invariants are binding safety rules enforced across sessions and workflows:

## Invariant 13: Debug Artifact Cleanup
- **Rule**: Prior to completing a session handoff, all temporary debug scripts, scratch logs, and transient build artifacts must be sanitized or removed.

## Invariant 14: Three Strikes Rule for Troubleshooting
- **Rule**: Maximum of **three attempts** allowed to solve a specific technical error or system failure. If the third attempt fails, all modification attempts MUST stop immediately and a Deep Research Request must be compiled.

## Invariant 24: Credential Leak Scan
- **Rule**: No plain-text API keys, passwords, private tokens, or secrets may be committed or persisted in public artifacts. A secret scan must run prior to session closing.

## Invariant 28: Mechanical Mitigations for Recurring Failures
- **Rule**: Any failure identified as RECURRING must be addressed using mechanical mitigations (strict MUST/MUST NOT directives or automated scripts), not advisory comments.

## Invariant 29: Script Verification Requirement
- **Rule**: After creating or modifying helper scripts (such as Python cleaner or audit scripts), the agent MUST run a syntax check/CLI test execution before marking the task complete.

## Invariant 30: Tool Name Schema Integrity
- **Rule**: Imported skills MUST be validated against active runtime tool declarations (`search_web`, `run_command`, `invoke_subagent`, etc.) before integration into global config.

## Invariant 31: Python Code Generation Safety
- **Rule**: The agent MUST NOT embed Python code containing quotes or regex patterns inside PowerShell `Set-Content @"..."@` heredocs. The agent MUST use native file tools or clean Python script writes to avoid string quote escaping corruption.

## Invariant 32: Strict Verification Exit Criteria
- **Rule**: When verifying script execution per Invariant 29, the agent MUST NOT accept CLI output with non-zero exit codes or usage/help messages as verification. The agent MUST execute scripts against a valid test input file and verify an exit code 0 result.

## Invariant 33: Sequential Operations Delegation Threshold
- **Rule**: The Orchestrator MUST NOT perform sequential script debugging, regex refactoring, or multi-file auditing on the main thread if the task requires more than 3 consecutive file/command operations. It MUST delegate these specialized tasks to subagents.

## Invariant 34: Orchestrator Delegation Mandate
- **Rule**: The Orchestrator agent coordinates tasks and must delegate heavy specialized sub-tasks (CVE scans, cleanups, deep research) to subagents.

## Invariant 35: Subagent Scope and Reporting
- **Rule**: Subagents operate within defined scope and must report findings back to the Orchestrator.

## Invariant 36: Invariant and Docstring Preservation
- **Rule**: Subagents must preserve existing codebase invariants and docstrings unless explicitly ordered to mutate them.

## Invariant 37: Subagent Command Exit Verification
- **Rule**: Subagents must verify all command exit codes before reporting task completion.

## Invariant 38: Three Strikes Deep Research Transition
- **Rule**: On the 3rd failed attempt to resolve a technical error or on user third-strike override, agents MUST halt all code mutations, compile all environment diagnostics, hypotheses, and logs into a structured Deep Research Request, and await verified research findings before resuming.

## Invariant 39: Evidence Before Assertions (Anti-Guessing)
- **Rule**: Agents MUST cite specific evidence (command output, API response, log line) before stating any root cause. Agents MUST NOT present a hypothesis as a conclusion or propose a fix in the same message as discovering an unverified issue.

## Invariant 40: Live Ground Truth Requirement (Anti-Stale Information)
- **Rule**: When evaluating active infrastructure state, switch port assignments, link health, or online status, agents MUST query live infrastructure (SSH, API, CLI, SNMP) rather than relying on static documentation or legacy CSV export dumps.

## Invariant 41: Anti-Simulated Tool Verification Gate
- **Rule**: Agents MUST NOT assert or imply browser rendering, UI interactions, or tool executions occurred unless backed by actual tool calls with live responses or captured image artifacts. If a tool fails to attach, agents MUST immediately notify the user.

## Invariant 42: Staged Deployment & Manual Upload Gate
- **Rule**: When deploying hardware configurations requiring manual or external staging (FTP, TFTP, file servers), file generation and device restart commands MUST be strictly decoupled into separate phases with an explicit user confirmation gate between them.

## Invariant 43: Ground Truth Entity & Acronym Verification
- **Rule**: Agents MUST verify institutional names, client identities, organization titles, and domain acronym expansions against authoritative repository documentation or device backups before scaffolding UI templates or configuration copy.

## Invariant 50: Production 3D Scene Hygiene & Diagnostic Quarantine
- **Rule**: Diagnostic rendering instrumentation, debug cameras, and temporary inspection shaders MUST be isolated in non-production scenes or strictly cleaned up prior to completion. Armature or mesh resets MUST purge obsolete datablocks so user-facing 3D deliverables remain free of ghost bones, wireframe pyramids, or diagnostic contamination.

## Invariant 51: Windows Interactive Desktop `WinSta0\Default` Routing & Physical HWND Visibility
- **Rule**: When launching interactive Windows GUI applications for user visibility, agents MUST route child processes explicitly to `WinSta0\Default` (`STARTUPINFO.lpDesktop = "WinSta0\\Default"`) and verify physical window visibility (`IsWindowVisible(hwnd)` and valid non-zero `GetWindowRect` coordinates on `WinSta0\Default`) before claiming the window is open on screen.

## Invariant 52: Fail-Safe Win32 Window Management & Process PID Scoping
- **Rule**: Win32 window enumeration and state mutation calls (`EnumWindows`, `ShowWindow`, `SetForegroundWindow`, `BringWindowToTop`) MUST strictly validate a non-null `target_pid > 0` (or explicit application window class), exclude OS system/shell window classes (`Progman`, `WorkerW`, `Shell_TrayWnd`, `IME`, `ApplicationFrameWindow`, etc.), and abort if matching window count exceeds the safety bound (5).

## Invariant 53: PowerShell Inline Python Escaping & Syntax Hardening
- **Rule**: Agents executing commands in Windows PowerShell (`pwsh`) MUST NOT pass complex inline Python (`python -c` with f-strings, nested quotes, semicolons, or multi-line statements) or bash-style `\"` quote escaping. Multi-statement or string-formatting Python logic MUST be written to a `.py` file and executed directly.

## Invariant 65: Single-Target Troubleshooting Isolation & Working Component Preservation
- **Rule**: When debugging an individual failing component, endpoint, or hardware unit alongside working components, agents MUST isolate modifications strictly to the failing target, use working components as read-only reference templates, and NEVER modify files the user confirmed are working without explicit user consent.

## Invariant 66: Real-Time User Narration, Output Hygiene & Anti-Silent Tool Chaining Gate
- **Rule**: Every tool-calling turn on the Orchestrator main thread MUST include a concise 1–2 sentence user-visible status update explaining the action being taken (including Turn 1 of `/wakeup` and every subsequent tool turn). Agents MUST NOT execute silent back-to-back tool-calling turns with empty user-facing text, and MUST NEVER emit internal chain-of-thought deliberation, meta-commentary, or unverified suffix tokens into the user-facing response stream.

## Invariant 67: Embedded Appliance Boot/GPT, Wi-Fi, Klipper Motion & Vendor IPC Daemon Safety
- **Rule**: On embedded SoC appliances (e.g. Creality K1 / Ingenic X2000E, Broadcom Wi-Fi), agents MUST NOT write raw MBR/Primary/Backup GPT sectors (`dd of=/dev/mmcblk*`, `sgdisk`, `sfdisk`, `parted`, `fdisk`, raw `r+b` opens). Factory GPT geometry warnings are benign and MUST NOT be "repaired". Agents MUST NOT disable, strip execute bits from, move, or comment out proprietary vendor boot/IPC daemons (`app-server`, `upgrade-server`, `audio-server`, `display-server`, `master-server`, `log_main`) without tracing the full boot/IPC chain first.
- Before mutating `/etc/init.d/*`, mount helpers, Wi-Fi auth/regulatory config, or network config on a remote headless appliance, agents MUST arm a host-side Dead-Man's Rollback Watchdog and verify the change with a cold service restart and a guarded cold reboot. Warm runtime state alone is not proof. On Klipper/CoreXY 3D printers, agents MUST NOT alter sensorless homing (`HOME_CUR` / `driver_SGTHRS`) without full-stroke `G28` verification, MUST NOT zero out `[prtouch_v2] noz_ex_com` when probing below printing temperature, MUST split G-code lines on `;` before injecting parameters, and MUST NOT run chamber exhaust fans at high speed during `ASA`/`ABS`/`PC`/`PA` prints. Full rule: `C:\Users\jmolina\.gemini\config\rules\embedded-boot-safety.md` §1-5, enforced mechanically by `pre_tool_guard.py`.

### Invariant 68: Bounded Subagent Lifecycle (<= 25 Steps / 180s), Unconditional 60s Heartbeat Timer, Telemetry Wakeup & Scope Slicing Gate (PAT-2026-1014-001 / GLB-017 / PM-2026-1026-001 / PM-2026-1026-008 / PM-2026-1102-007)
- **Rule**: Every `invoke_subagent` prompt MUST bound the subagent to at most **25 tool steps or 3 minutes (`180s`)** before reporting via `send_message` (enforced mechanically by `pre_tool_guard.py` on `invoke_subagent`), defaulting to `<= 12–15 tool steps / <= 90s` for micro-subagents (`<= 25 steps / <= 180s` hard ceiling). Prompts MUST scope subagents to at most `1–3 target files` (`<= 400 LOC`) and `<= 4 numbered task items` under the **Atomic Micro-Subagent Fan-Out & Scope Slicing Gate (`PM-2026-1102-007` / `GLB-020`)**, include `Parent Conversation ID: <id>` whenever mid-run heartbeats are needed, and forbid open-ended rabbit holes or redundant `send_message` wrap-ups to subagents that have already returned their report. On **every** turn where the Orchestrator calls `invoke_subagent` or sends work to a running subagent via `send_message`, the Orchestrator MUST arm at most **one** non-conflicting `schedule(DurationSeconds=60, TimerCondition="any")` timer in that same turn (verifying via `manage_task(action="list")` first). When a `60s` watchdog timer fires and a subagent is still running, the Orchestrator MUST run `python 'C:\Users\jmolina\.gemini\config\skills\agent-behavior-auditor\scripts\parse_transcript.py' --heartbeat <subagent_transcript_path>` (exempt from the 3-op cap) alongside `manage_subagents(action="list")` to emit concrete step/tool telemetry to the user—while raw `read_file`/`view_file` on `transcript.jsonl` remains strictly forbidden (`GLB-017`). If the subagent has exceeded `25 steps` or `180s`, the Orchestrator MUST intervene via `send_message` or kill/replace it and clean up dead partial scripts in `scratch/`.

### Invariant 69: User-Intent Preservation & Anti-Feature-Stripping Gate (PAT-2026-1014-002 / PM-2026-1026-002)
- **Rule**: Agents MUST NOT resolve a bug, calibration issue, or test failure by silently disabling or downgrading an enabled user feature (e.g., switching `ADAPTIVE_BED_MESH` to `FULL_BED_MESH`, disabling `transfer_call`, or stripping mobile call forwarding) without explicit prior user consent and prominent disclosure. Full rule: `C:\Users\jmolina\.gemini\config\rules\verify-autonomous-work.md`.

### Invariant 70: Mandatory Terminal Proposal Summary Gate (GLB-023 / PM-2026-1106-001 / IMP-2026-1106-001)
- **MUST**: Every final response that asks the user for permission or stops working MUST end with a structured `### Proposal Summary` block as the last thing printed to the screen (enumerating `#`, `Target System`, `Proposed Action`, `Scope / Affected Entities`, `Command / Mechanism`, and `Mode`, or explicitly stating `None — Task 100% complete` when zero actions remain per Invariant 65).
- **MUST NOT**: Never bury permission requests in prose subsections or print additional content after the terminal Proposal Summary and approval prompt. Full rule: `C:\Users\jmolina\.gemini\config\rules\orchestrator-delegation-and-narration.md`.

## Historical / Retired Global Invariants Index
The following global invariant numbers have been retired, merged, or consolidated into active Invariants 13–70 and global rules in `C:\Users\jmolina\.gemini\config\rules\*.md` during the 2026-08 and 2026-10 governance unifications. Their numbers are retained and never reused:
- **Invariants 1–12**: Consolidated into core workspace bootstrap and session hygiene protocols (now tracked in `wakeup_bootstrap.py`, `/wakeup`, `/goodbye`, and Invariant 13).
- **Invariants 15–23**: Consolidated into autonomous troubleshooting, verification, and artifact isolation rules (`single-target-troubleshooting.md`, `verify-autonomous-work.md`, Invariants 14, 24, 28, 29).
- **Invariants 25–27**: Consolidated into operational tool hygiene and environment validation contracts (Invariants 28–32).
- **Invariant 38**: Merged into Invariant 14 (Three Strikes Deep Research Transition).
- **Invariants 44–49**: Consolidated into physical hardware, staged deployment, and live ground truth verification suites (`no-guessing.md`, Invariants 39–43).
- **Invariants 54–64**: Consolidated into interactive GUI automation, terminal quoting, subagent lifecycle controls, and narration gates (`powershell-syntax-hardening.md`, `orchestrator-delegation-and-narration.md`, Invariants 50–53, 65–68).
