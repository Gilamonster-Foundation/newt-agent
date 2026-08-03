---- MODULE PromptControls ----

CONSTANTS Actions, Displayed
ASSUME /\ Displayed \subseteq Actions
       /\ "none" \notin Actions

VARIABLE mode, decision, last, reader
vars == <<mode, decision, last, reader>>

Init == /\ mode = "prompt"
        /\ decision = "none"
        /\ last = "none"
        /\ reader = "live"

\* Local terminal controls require a LIVE control reader. A transient reader
\* error (ReaderError below) does not permanently disable them: ReaderRecover
\* re-arms the reader and these actions become enabled again.
Submit(action) ==
  /\ mode = "prompt" /\ reader = "live" /\ action \in Displayed
  /\ mode' = "chat" /\ decision' = action /\ last' = "submit" /\ reader' = reader

Esc ==
  /\ mode = "prompt" /\ reader = "live"
  /\ mode' = "chat" /\ decision' = "none" /\ last' = "esc" /\ reader' = reader

CtrlC ==
  /\ mode = "prompt" /\ reader = "live"
  /\ mode' = "exited" /\ decision' = "none" /\ last' = "ctrl-c" /\ reader' = reader

CtrlD ==
  /\ mode = "prompt" /\ reader = "live"
  /\ mode' = "exited" /\ decision' = "none" /\ last' = "ctrl-d" /\ reader' = reader

\* The web decision path is reader-INDEPENDENT: it stays available while the
\* local control reader is retrying. Its ONLY resolution guard is mode = "prompt"
\* — the shared guard that makes a local abort and a web decision mutually
\* exclusive (whichever fires first moves mode out of "prompt").
WebSubmit(action) ==
  /\ mode = "prompt" /\ action \in Displayed
  /\ mode' = "chat" /\ decision' = action /\ last' = "web-submit" /\ reader' = reader

\* A transient control-reader error: it neither authorizes nor resolves the
\* prompt. It only degrades the reader from "live" to "retrying".
ReaderError ==
  /\ mode = "prompt" /\ reader = "live"
  /\ reader' = "retrying"
  /\ mode' = mode /\ decision' = decision /\ last' = "reader-error"

\* Re-arm the control reader after a transient error, restoring local controls.
ReaderRecover ==
  /\ mode = "prompt" /\ reader = "retrying"
  /\ reader' = "live"
  /\ mode' = mode /\ decision' = decision /\ last' = "reader-recover"

\* The fail-closed web-decision timeout. It is reader-INDEPENDENT (fires while
\* live OR retrying), so an unresolved prompt ALWAYS has a resolving transition
\* available even with no web input and a dead reader — the deadline denies
\* without authorizing. This is what makes a stuck reader unable to strand the
\* prompt.
Timeout ==
  /\ mode = "prompt"
  /\ mode' = "chat" /\ decision' = "none" /\ last' = "timeout" /\ reader' = reader

Done == /\ mode \in {"chat", "exited"}
        /\ UNCHANGED vars

\* Next quantifies over ALL of Actions — including the undisplayed decoy — so
\* the `action \in Displayed` guard INSIDE Submit/WebSubmit is the single
\* load-bearing check (mutation-tested: weakening either operator's guard to
\* `action \in Actions` violates AuthorizationDisplayed). Bounding the
\* existentials to Displayed instead would mask exactly that mutation.
Next == \/ \E action \in Actions : Submit(action)
        \/ \E action \in Actions : WebSubmit(action)
        \/ Esc \/ CtrlC \/ CtrlD
        \/ ReaderError \/ ReaderRecover \/ Timeout
        \/ Done

Spec == Init /\ [][Next]_vars

TypeOK == /\ mode \in {"prompt", "chat", "exited"}
          /\ decision \in Actions \cup {"none"}
          /\ last \in {"none", "submit", "web-submit", "esc", "ctrl-c", "ctrl-d",
                       "reader-error", "reader-recover", "timeout"}
          /\ reader \in {"live", "retrying"}

AuthorizationDisplayed == decision = "none" \/ decision \in Displayed
EscCancels == last = "esc" => mode = "chat" /\ decision = "none"
ControlsExit == last \in {"ctrl-c", "ctrl-d"} =>
                  mode = "exited" /\ decision = "none"
ExitIsTerminal == [](mode = "exited" => [](mode = "exited"))

\* A reader error never authorizes and never resolves the prompt.
ReaderErrorNeverAuthorizes ==
  last = "reader-error" => (mode = "prompt" /\ decision = "none")

\* Recovery re-arms the local controls: after it, mode is still "prompt" and the
\* reader is "live", so Esc / Ctrl-C / Ctrl-D are reachable again (the reader
\* failure was not a permanent dead end).
RecoveryReArmsControls ==
  last = "reader-recover" => (mode = "prompt" /\ reader = "live")

\* A local abort and a submitted decision cannot BOTH win: once the prompt is
\* resolved, the outcome is EITHER an authorized (displayed) decision OR an
\* abort — never both. (While still "prompt", nothing has won yet.)
SingleWinner ==
  \/ mode = "prompt"
  \/ (decision \in Displayed /\ last \in {"submit", "web-submit"})
  \/ (decision = "none" /\ last \in {"esc", "ctrl-c", "ctrl-d", "timeout"})

\* The timeout resolves fail-closed: it never authorizes an action.
TimeoutNeverAuthorizes == last = "timeout" => decision = "none"

====
