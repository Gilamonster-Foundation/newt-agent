---- MODULE PromptControls ----

CONSTANTS Actions, Displayed
ASSUME /\ Displayed \subseteq Actions
       /\ "none" \notin Actions

VARIABLE mode, decision, last
vars == <<mode, decision, last>>

Init == /\ mode = "prompt"
        /\ decision = "none"
        /\ last = "none"

Submit(action) ==
  /\ mode = "prompt" /\ action \in Displayed
  /\ mode' = "chat" /\ decision' = action /\ last' = "submit"

Esc ==
  /\ mode = "prompt"
  /\ mode' = "chat" /\ decision' = "none" /\ last' = "esc"

CtrlC ==
  /\ mode = "prompt"
  /\ mode' = "exited" /\ decision' = "none" /\ last' = "ctrl-c"

CtrlD ==
  /\ mode = "prompt"
  /\ mode' = "exited" /\ decision' = "none" /\ last' = "ctrl-d"

Done == /\ mode \in {"chat", "exited"}
        /\ UNCHANGED vars

Next == \/ \E action \in Displayed : Submit(action)
        \/ Esc \/ CtrlC \/ CtrlD \/ Done

Spec == Init /\ [][Next]_vars

TypeOK == /\ mode \in {"prompt", "chat", "exited"}
          /\ decision \in Actions \cup {"none"}
          /\ last \in {"none", "submit", "esc", "ctrl-c", "ctrl-d"}

AuthorizationDisplayed == decision = "none" \/ decision \in Displayed
EscCancels == last = "esc" => mode = "chat" /\ decision = "none"
ControlsExit == last \in {"ctrl-c", "ctrl-d"} =>
                  mode = "exited" /\ decision = "none"
ExitIsTerminal == [](mode = "exited" => [](mode = "exited"))

====

=============================================================================
