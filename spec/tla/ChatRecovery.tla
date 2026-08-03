-------------------------- MODULE ChatRecovery --------------------------
\* The temporal contract for chat-loop dispatch recovery (#1533 / epic #1529).
\*
\* A dispatch RECOVERY (context-window 400, tools-unsupported, malformed-XML)
\* must retry the SAME logical round; only a COMPLETED dispatch may consume a
\* round; the tools-disabled summary begins ONLY after the EFFECTIVE round cap
\* completes; an exhausted recovery budget terminates explicitly (never falls
\* through into a summary). HTTP / model output are abstract — no JSON, reqwest,
\* or wiremock is modeled. The historic #1533 bug (a recovery `continue`-ing the
\* OUTER round) is the trace [round 0: Cw400 ; round 1: summary], which violates
\* RetryObligationIsSameRound + SummaryOnlyAfterCompletedCap.
\*
\* `roundCap` is the EFFECTIVE logical-round cap — the configured `max_tool_rounds`
\* PLUS any explicitly granted workflow-grace window (the production loops extend
\* the soft cap via `current_tool_round_limit`). This model does NOT model the
\* grace-selection mechanism; it abstracts the cap to a single value chosen once
\* per behavior from `RoundCaps`, so ONE TLC run explores every cap (the one-round
\* crucible AND the multi-round case) — no second .cfg to drift out of sync.
EXTENDS Naturals, Sequences, TLC

CONSTANTS RoundCaps, MaxCwRetries, MaxXmlRetries

ASSUME /\ RoundCaps \subseteq Nat
       /\ RoundCaps # {}
       /\ \A cap \in RoundCaps : cap > 0
       /\ MaxCwRetries \in Nat
       /\ MaxXmlRetries \in Nat

Responses ==
    {"Cw400",
     "ToolsUnsupported",
     "MalformedXml",
     "ToolCalls",
     "FinalText",
     "Fatal"}

RecoveryKinds == {"CwRetry", "ToolsRetry", "XmlRetry"}

Phases ==
    {"Ready",
     "Handle",
     "SummaryReady",
     "SummaryHandle",
     "Done",
     "Error"}

VARIABLES
    phase,
    logicalRound,
    roundCap,
    toolsEnabled,
    cwRetries,
    xmlRetries,
    pending,
    requests,
    retryPending,
    retryRound,
    retryKind

vars ==
    << phase,
       logicalRound,
       roundCap,
       toolsEnabled,
       cwRetries,
       xmlRetries,
       pending,
       requests,
       retryPending,
       retryRound,
       retryKind >>

\* Sequence -> set of its elements. TLC has no built-in; the module defines it
\* so the invariants below can quantify over the request log as a set.
SeqToSet(s) == { s[i] : i \in DOMAIN s }

Request(summary, cause) ==
    [ logicalRound |-> logicalRound,
      tools       |-> IF summary THEN FALSE ELSE toolsEnabled,
      summary     |-> summary,
      cause       |-> cause ]

Init ==
    /\ phase = "Ready"
    /\ logicalRound = 0
    \* The effective cap is chosen once per behavior; TLC explores all of RoundCaps
    \* (e.g. {1, 2}) in a single run — the one-round crucible is a real merge gate.
    /\ roundCap \in RoundCaps
    /\ toolsEnabled = TRUE
    /\ cwRetries = 0
    /\ xmlRetries = 0
    /\ pending = "None"
    /\ requests = <<>>
    /\ retryPending = FALSE
    /\ retryRound = 0
    /\ retryKind = "None"

SendToolRequest ==
    /\ phase = "Ready"
    /\ logicalRound < roundCap
    /\ (~retryPending \/ logicalRound = retryRound)
    /\ \E response \in Responses:
        /\ requests' =
             Append(
                 requests,
                 Request(
                     FALSE,
                     IF retryPending THEN retryKind ELSE "Normal"
                 )
             )
        /\ pending' = response
        /\ phase' = "Handle"
        /\ retryPending' = FALSE
        /\ UNCHANGED
             << logicalRound,
                roundCap,
                toolsEnabled,
                cwRetries,
                xmlRetries,
                retryRound,
                retryKind >>

RecoverCw400 ==
    /\ phase = "Handle"
    /\ pending = "Cw400"
    /\ cwRetries < MaxCwRetries
    /\ cwRetries' = cwRetries + 1
    /\ retryPending' = TRUE
    /\ retryRound' = logicalRound
    /\ retryKind' = "CwRetry"
    /\ phase' = "Ready"
    /\ pending' = "None"
    /\ UNCHANGED
         << logicalRound,
            roundCap,
            toolsEnabled,
            xmlRetries,
            requests >>

RecoverUnsupportedTools ==
    /\ phase = "Handle"
    /\ pending = "ToolsUnsupported"
    /\ toolsEnabled
    /\ toolsEnabled' = FALSE
    /\ retryPending' = TRUE
    /\ retryRound' = logicalRound
    /\ retryKind' = "ToolsRetry"
    /\ phase' = "Ready"
    /\ pending' = "None"
    /\ UNCHANGED
         << logicalRound,
            roundCap,
            cwRetries,
            xmlRetries,
            requests >>

RecoverMalformedXml ==
    /\ phase = "Handle"
    /\ pending = "MalformedXml"
    /\ toolsEnabled
    /\ xmlRetries < MaxXmlRetries
    /\ xmlRetries' = xmlRetries + 1
    /\ retryPending' = TRUE
    /\ retryRound' = logicalRound
    /\ retryKind' = "XmlRetry"
    /\ phase' = "Ready"
    /\ pending' = "None"
    /\ UNCHANGED
         << logicalRound,
            roundCap,
            toolsEnabled,
            cwRetries,
            requests >>

CompleteToolRound ==
    /\ phase = "Handle"
    /\ pending = "ToolCalls"
    /\ logicalRound' = logicalRound + 1
    /\ phase' =
         IF logicalRound + 1 = roundCap
         THEN "SummaryReady"
         ELSE "Ready"
    /\ pending' = "None"
    /\ retryPending' = FALSE
    /\ retryKind' = "None"
    /\ UNCHANGED
         << roundCap,
            toolsEnabled,
            cwRetries,
            xmlRetries,
            requests,
            retryRound >>

CompleteFinalText ==
    /\ phase = "Handle"
    /\ pending = "FinalText"
    /\ phase' = "Done"
    /\ pending' = "None"
    /\ UNCHANGED
         << logicalRound,
            roundCap,
            toolsEnabled,
            cwRetries,
            xmlRetries,
            requests,
            retryPending,
            retryRound,
            retryKind >>

RecoveryExhausted ==
    /\ phase = "Handle"
    /\ \/ /\ pending = "Cw400"
          /\ cwRetries >= MaxCwRetries
       \/ /\ pending = "MalformedXml"
          /\ (~toolsEnabled \/ xmlRetries >= MaxXmlRetries)
       \/ /\ pending = "ToolsUnsupported"
          /\ ~toolsEnabled
       \/ pending = "Fatal"
    /\ phase' = "Error"
    /\ pending' = "None"
    /\ UNCHANGED
         << logicalRound,
            roundCap,
            toolsEnabled,
            cwRetries,
            xmlRetries,
            requests,
            retryPending,
            retryRound,
            retryKind >>

SendSummary ==
    /\ phase = "SummaryReady"
    /\ logicalRound = roundCap
    /\ \E response \in {"FinalText", "Fatal"}:
        /\ requests' =
             Append(requests, Request(TRUE, "Summary"))
        /\ pending' = response
        /\ phase' = "SummaryHandle"
        /\ UNCHANGED
             << logicalRound,
                roundCap,
                toolsEnabled,
                cwRetries,
                xmlRetries,
                retryPending,
                retryRound,
                retryKind >>

CompleteSummary ==
    /\ phase = "SummaryHandle"
    /\ pending = "FinalText"
    /\ phase' = "Done"
    /\ pending' = "None"
    /\ UNCHANGED
         << logicalRound,
            roundCap,
            toolsEnabled,
            cwRetries,
            xmlRetries,
            requests,
            retryPending,
            retryRound,
            retryKind >>

FailSummary ==
    /\ phase = "SummaryHandle"
    /\ pending = "Fatal"
    /\ phase' = "Error"
    /\ pending' = "None"
    /\ UNCHANGED
         << logicalRound,
            roundCap,
            toolsEnabled,
            cwRetries,
            xmlRetries,
            requests,
            retryPending,
            retryRound,
            retryKind >>

\* The intended terminal states stutter, so TLC's deadlock check does not flag
\* designed termination (Done/Error, which have no dispatch successor) as a stuck
\* state — a GENUINE non-terminal deadlock is still caught. Termination
\* (<>(phase \in {Done, Error})) is unaffected: once terminal, it stays terminal.
Terminated ==
    /\ phase \in {"Done", "Error"}
    /\ UNCHANGED vars

Next ==
    \/ SendToolRequest
    \/ RecoverCw400
    \/ RecoverUnsupportedTools
    \/ RecoverMalformedXml
    \/ CompleteToolRound
    \/ CompleteFinalText
    \/ RecoveryExhausted
    \/ SendSummary
    \/ CompleteSummary
    \/ FailSummary
    \/ Terminated

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(Next)

TypeOK ==
    /\ phase \in Phases
    /\ roundCap \in RoundCaps
    /\ logicalRound \in 0..roundCap
    /\ toolsEnabled \in BOOLEAN
    /\ cwRetries \in Nat
    /\ xmlRetries \in Nat
    /\ pending \in Responses \cup {"None"}
    /\ requests \in Seq(
         [ logicalRound : Nat,
           tools : BOOLEAN,
           summary : BOOLEAN,
           cause : STRING ]
       )
    /\ retryPending \in BOOLEAN
    /\ retryRound \in Nat
    /\ retryKind \in RecoveryKinds \cup {"None"}

RetryObligationIsSameRound ==
    retryPending =>
        /\ phase = "Ready"
        /\ logicalRound = retryRound
        /\ logicalRound < roundCap

RecoveryRequestStaysInRound ==
    \A i \in 2..Len(requests):
        requests[i].cause \in RecoveryKinds =>
            /\ requests[i].logicalRound =
                 requests[i - 1].logicalRound
            /\ ~requests[i].summary

SummaryOnlyAfterCompletedCap ==
    \A request \in SeqToSet(requests):
        request.summary =>
            /\ request.logicalRound = roundCap
            /\ ~request.tools

UnsupportedToolsRetryDisablesTools ==
    \A request \in SeqToSet(requests):
        request.cause = "ToolsRetry" =>
            ~request.tools

MalformedXmlRetryRetainsTools ==
    \A request \in SeqToSet(requests):
        request.cause = "XmlRetry" =>
            request.tools

RecoveryBounds ==
    /\ cwRetries <= MaxCwRetries
    /\ xmlRetries <= MaxXmlRetries

Termination ==
    <>(phase \in {"Done", "Error"})

=============================================================================
