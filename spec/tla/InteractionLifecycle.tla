---- MODULE InteractionLifecycle ----
\* The A3 interaction controller's lifecycle and exactly-once resolution
\* (BHV-INTERACTION-004..006 in ../behavior-map.toml).
\*
\* Models `newt-interaction/src/instance.rs::LifecycleState` together with the
\* CAS in `newt-core/src/interaction_resolution.rs::resolve`: N concurrent
\* responders race to resolve ONE offer, each presenting a response id and an
\* idempotency key. The store's `ON CONFLICT(instance_id) DO NOTHING` insert
\* makes the rowcount the whole decision, and the loser then reads the winner
\* inside the same transaction.
\*
\* Responders are modelled GENERICALLY (N racers), deliberately unlike
\* `PromptControls`'s binary `SingleWinner`: `Audience` is `#[non_exhaustive]`
\* and a third variant must not silently invalidate this model.

EXTENDS Naturals, FiniteSets

CONSTANTS Responders,   \* the racing responders (N >= 3 in the cfg)
          Responses,    \* distinct response ids they may present
          Keys          \* idempotency keys

ASSUME /\ Responders # {} /\ Responses # {} /\ Keys # {}
       /\ "none" \notin Responses /\ "none" \notin Keys

VARIABLES state,      \* the offer's lifecycle state
          winner,     \* the response that resolved it, or "none"
          winnerKey,  \* the idempotency key that won, or "none"
          sealed,     \* WRITE-ONCE: the FIRST terminal state reached, or "none"
          outcome,    \* Responder -> what the controller told it
          observed,   \* Responder -> the winner it was told about, or "none"
          presented   \* Responder -> what it submitted

vars == <<state, winner, winnerKey, sealed, outcome, observed, presented>>

Terminal == {"Answered", "Cancelled", "Expired", "Unsupported"}
States == {"Draft", "Published"} \cup Terminal
Outcomes == {"pending", "won", "lost", "replayed", "conflict", "refused"}
NoSubmission == [resp |-> "none", key |-> "none"]

Init == /\ state = "Draft"
        /\ winner = "none"
        /\ winnerKey = "none"
        /\ sealed = "none"
        /\ outcome = [r \in Responders |-> "pending"]
        /\ observed = [r \in Responders |-> "none"]
        /\ presented = [r \in Responders |-> NoSubmission]

\* Draft -> Published. The host mint; `transition` cannot reach it.
Publish ==
  /\ state = "Draft" /\ sealed = "none"
  /\ state' = "Published"
  /\ UNCHANGED <<winner, winnerKey, sealed, outcome, observed, presented>>

\* A non-terminal offer moves to a terminal state. `state \in {"Draft",
\* "Published"}` is the load-bearing check for TerminalIsTerminal: weakening it
\* lets a terminal offer transition again, and because `sealed` is WRITE-ONCE
\* the invariant sees the contradiction.
Close(to) ==
  /\ state \in {"Draft", "Published"}
  /\ to \in {"Cancelled", "Expired", "Unsupported"}
  /\ state' = to
  /\ sealed' = IF sealed = "none" THEN to ELSE sealed
  /\ UNCHANGED <<winner, winnerKey, outcome, observed, presented>>

\* One responder presents (resp, key). Mirrors `resolve`'s branch order
\* exactly: win on an empty slot, else replay when the STORED WINNER is the
\* same response, else conflict when the stored KEY is the same but the
\* response differs, else lose.
\*
\* The CAS is on the RESOLUTION, not on the lifecycle state, exactly as the
\* implementation has it: `ON CONFLICT(instance_id) DO NOTHING` against
\* `interaction_resolutions`. Two racers can both have validated against a
\* Published offer and both call `resolve`; the loser still reads the winner.
\* So the race stays enabled after the offer becomes Answered, and
\* `winner = "none"` is the SINGLE guard deciding who resolves it.

\* Closed without an answer: nothing may authorize from here.
Closed == sealed \in {"Cancelled", "Expired", "Unsupported"}
\* Never offered yet.
NotYetOpen == state = "Draft"

Attempt(r, resp, key) ==
  /\ outcome[r] = "pending"
  /\ presented' = [presented EXCEPT ![r] = [resp |-> resp, key |-> key]]
  /\ \/ \* the offer is not open: refused, and NOTHING is authorized.
        \* `~Closed` is the load-bearing check for ExpiryNeverAuthorizes.
        /\ Closed \/ NotYetOpen
        /\ outcome' = [outcome EXCEPT ![r] = "refused"]
        /\ observed' = [observed EXCEPT ![r] = "none"]
        /\ UNCHANGED <<state, winner, winnerKey, sealed>>
     \/ \* the CAS succeeds: this response resolves the offer, exactly once.
        \* `winner = "none"` is the load-bearing check for ExactlyOneResolution.
        /\ ~Closed /\ ~NotYetOpen
        /\ winner = "none"
        /\ winner' = resp /\ winnerKey' = key
        /\ state' = "Answered"
        /\ sealed' = IF sealed = "none" THEN "Answered" ELSE sealed
        /\ outcome' = [outcome EXCEPT ![r] = "won"]
        /\ observed' = [observed EXCEPT ![r] = resp]
     \/ \* already resolved by the SAME response: an idempotent replay.
        \* `resp = winner` is the load-bearing check for IdempotentRetryCollapses.
        /\ ~Closed /\ ~NotYetOpen
        /\ winner # "none" /\ resp = winner
        /\ outcome' = [outcome EXCEPT ![r] = "replayed"]
        /\ observed' = [observed EXCEPT ![r] = winner]
        /\ UNCHANGED <<state, winner, winnerKey, sealed>>
     \/ \* same key, DIFFERENT response: refused, never substituted.
        \* `winnerKey = key` is the load-bearing check for ConflictingKeyRefused.
        /\ ~Closed /\ ~NotYetOpen
        /\ winner # "none" /\ resp # winner
        /\ winnerKey = key
        /\ outcome' = [outcome EXCEPT ![r] = "conflict"]
        /\ observed' = [observed EXCEPT ![r] = winner]
        /\ UNCHANGED <<state, winner, winnerKey, sealed>>
     \/ \* a different response under a different key: an ordinary loser.
        /\ ~Closed /\ ~NotYetOpen
        /\ winner # "none" /\ resp # winner
        /\ winnerKey # key
        /\ outcome' = [outcome EXCEPT ![r] = "lost"]
        /\ observed' = [observed EXCEPT ![r] = winner]
        /\ UNCHANGED <<state, winner, winnerKey, sealed>>

\* A resolved offer is still readable: a responder that already has an answer
\* stutters rather than deadlocking.
Done ==
  /\ \A r \in Responders : outcome[r] # "pending"
  /\ state \in Terminal
  /\ UNCHANGED vars

Next == \/ Publish
        \/ \E to \in {"Cancelled", "Expired", "Unsupported"} : Close(to)
        \/ \E r \in Responders, resp \in Responses, key \in Keys : Attempt(r, resp, key)
        \/ Done

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ state \in States
  /\ winner \in Responses \cup {"none"}
  /\ winnerKey \in Keys \cup {"none"}
  /\ sealed \in Terminal \cup {"none"}
  /\ outcome \in [Responders -> Outcomes]
  /\ observed \in [Responders -> Responses \cup {"none"}]
  /\ presented \in [Responders -> [resp : Responses \cup {"none"},
                                   key : Keys \cup {"none"}]]

\* At most one response ever resolves the offer: at most one responder is told
\* it won, and a winner exists exactly when one was.
ExactlyOneResolution ==
  /\ Cardinality({r \in Responders : outcome[r] = "won"}) <= 1
  /\ (winner # "none") <=> (\E r \in Responders : outcome[r] = "won")

\* Every non-winner that got an answer observed the SAME winner the winner
\* produced — never a stale, empty, or contradictory one.
LoserObservesTerminal ==
  \A r \in Responders :
    outcome[r] \in {"lost", "replayed", "conflict"} =>
      /\ winner # "none"
      /\ observed[r] = winner
      /\ state = "Answered"

\* No transition leaves a terminal state, once reached.
TerminalIsTerminal ==
  /\ (sealed # "none") => (state = sealed)
  /\ (state \in Terminal) => (sealed = state)

\* An expired (or otherwise closed-without-answer) offer authorizes nothing:
\* no winner, and nobody was told they won.
ExpiryNeverAuthorizes ==
  Closed =>
    /\ winner = "none"
    /\ \A r \in Responders : outcome[r] # "won"

\* The same response presented again collapses to a replay rather than a second
\* resolution — and a replay is reported EXACTLY when the presented response is
\* the winner.
IdempotentRetryCollapses ==
  \A r \in Responders :
    /\ (outcome[r] = "replayed") =>
         (presented[r].resp = winner /\ winner # "none")
    /\ (outcome[r] \in {"lost", "conflict"}) => presented[r].resp # winner

\* The same key with a DIFFERENT response is refused, never substituted: the
\* conflicting responder's submission did not become the winner, and the
\* winning key is unchanged.
ConflictingKeyRefused ==
  \A r \in Responders :
    (outcome[r] = "conflict") =>
      /\ presented[r].key = winnerKey
      /\ presented[r].resp # winner
      /\ winner # "none"

====
