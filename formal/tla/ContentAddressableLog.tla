---------------------- MODULE ContentAddressableLog ----------------------
(***************************************************************************)
(* The content-addressable first principle under CONCURRENCY and CRASH.    *)
(*                                                                         *)
(* The Lean kernel (formal/ContentAddressed/Basic.lean) proves the         *)
(* structural laws: append preserves the prefix, edit-as-append keeps the  *)
(* original, tampering breaks the chain given an injective address         *)
(* function, and — the load-bearing one — that unverified evidence is      *)
(* indistinguishable from no evidence.                                     *)
(*                                                                         *)
(* Lean cannot reach the parts that only exist in time: two writers        *)
(* racing, a process dying between "wrote the bytes" and "committed the    *)
(* entry", a reader observing a log mid-append. Those are what this        *)
(* module checks.                                                          *)
(*                                                                         *)
(* THE THREE OBLIGATIONS                                                   *)
(*                                                                         *)
(*   NoTornEntry     a crash never leaves a partially-written entry        *)
(*                   visible to a reader. Either the entry is committed    *)
(*                   whole or it is not there at all.                      *)
(*                                                                         *)
(*   AppendOnly      the committed log only ever grows by suffix. No       *)
(*                   interleaving of concurrent writers can shorten it,    *)
(*                   reorder it, or alter an already-committed entry.      *)
(*                                                                         *)
(*   AddressMatches  every committed entry's id equals the hash of its     *)
(*                   content. An entry whose id does not name its own      *)
(*                   bytes is not content-addressed, it is merely labelled *)
(*                   — the failure this whole principle exists to exclude. *)
(*                                                                         *)
(* WHAT IS DELIBERATELY NOT CHECKED HERE                                   *)
(*                                                                         *)
(*   Hash collision resistance. Hash(c) is modelled as an injective        *)
(*   function by construction, exactly as Lean carries `hinj` as an        *)
(*   explicit hypothesis. Neither artifact proves a real hash is sound;    *)
(*   both prove what follows GIVEN that it is. Saying so is the point —    *)
(*   a reader who skimmed could otherwise think integrity was established  *)
(*   outright.                                                             *)
(*                                                                         *)
(*   Durability of the underlying medium. If fsync lies, nothing above it  *)
(*   can be true. That is an assumption about hardware, not a property of  *)
(*   this design.                                                          *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Writers,      \* set of concurrent writer identities
    Contents,     \* set of distinct content values that may be appended
    MaxLen        \* bound the log so TLC terminates

\* Addressing is a function, so equal content necessarily has equal address.
\* Injectivity is the modelled assumption (see the boundary note above).
Hash(c) == c

VARIABLES
    committed,    \* Seq of records [id |-> Addr, content |-> Content, prev |-> Addr]
    staged,       \* [Writers -> record \cup {NoStage}] work in flight, not yet visible
    crashed       \* set of writers that died mid-write

NoStage == [id |-> 0, content |-> 0, prev |-> 0, valid |-> FALSE]

vars == <<committed, staged, crashed>>

Genesis == 0

LastAddr ==
    IF Len(committed) = 0 THEN Genesis ELSE committed[Len(committed)].id

TypeOK ==
    /\ committed \in Seq([id: Nat, content: Nat, prev: Nat])
    /\ staged \in [Writers -> [id: Nat, content: Nat, prev: Nat, valid: BOOLEAN]]
    /\ crashed \subseteq Writers

Init ==
    /\ committed = << >>
    /\ staged = [w \in Writers |-> NoStage]
    /\ crashed = {}

(***************************************************************************)
(* Stage: a writer prepares an entry. This models the window where bytes   *)
(* exist but nothing is visible to a reader. Crashing here must leave no   *)
(* trace — that is what NoTornEntry checks.                                *)
(***************************************************************************)
Stage(w, c) ==
    /\ w \notin crashed
    /\ staged[w].valid = FALSE
    /\ Len(committed) < MaxLen
    /\ staged' = [staged EXCEPT ![w] =
                    [id |-> Hash(c), content |-> c, prev |-> LastAddr, valid |-> TRUE]]
    /\ UNCHANGED <<committed, crashed>>

(***************************************************************************)
(* Commit: the single atomic step that makes an entry visible. The `prev`  *)
(* is RE-READ at commit time, not reused from staging — a writer that      *)
(* staged against a stale tail must re-link rather than fork the chain.    *)
(* Dropping that re-read is precisely how a concurrent implementation      *)
(* silently produces two entries claiming the same predecessor.            *)
(***************************************************************************)
Commit(w) ==
    /\ w \notin crashed
    /\ staged[w].valid = TRUE
    /\ Len(committed) < MaxLen
    /\ committed' = Append(committed,
                        [id |-> staged[w].id,
                         content |-> staged[w].content,
                         prev |-> LastAddr])
    /\ staged' = [staged EXCEPT ![w] = NoStage]
    /\ UNCHANGED crashed

(***************************************************************************)
(* Crash: a writer dies. Anything it staged evaporates. Nothing            *)
(* half-written becomes visible.                                           *)
(***************************************************************************)
Crash(w) ==
    /\ w \notin crashed
    /\ crashed' = crashed \cup {w}
    /\ staged' = [staged EXCEPT ![w] = NoStage]
    /\ UNCHANGED committed

(***************************************************************************)
(* Terminating: every writer has crashed, or the log is full. This is a    *)
(* legitimate end state, not a deadlock — modelled explicitly so TLC does  *)
(* not report reaching it as a failure.                                    *)
(***************************************************************************)
Terminating ==
    /\ \/ crashed = Writers
       \/ Len(committed) = MaxLen
    /\ UNCHANGED vars

Next ==
    \/ \E w \in Writers, c \in Contents : Stage(w, c)
    \/ \E w \in Writers : Commit(w)
    \/ \E w \in Writers : Crash(w)
    \/ Terminating

Spec == Init /\ [][Next]_vars /\ WF_vars(\E w \in Writers : Commit(w))

(***************************************************************************)
(* INVARIANTS                                                              *)
(***************************************************************************)

\* Every committed entry names its own bytes.
AddressMatches ==
    \A i \in 1..Len(committed) : committed[i].id = Hash(committed[i].content)

\* The chain is intact: each entry links to the one before it.
ChainIntact ==
    \A i \in 1..Len(committed) :
        committed[i].prev = IF i = 1 THEN Genesis ELSE committed[i-1].id

\* A crash never exposes a partial entry: everything visible is well-formed.
NoTornEntry ==
    \A i \in 1..Len(committed) :
        /\ committed[i].id \in Nat
        /\ committed[i].content \in Contents
        /\ committed[i].id = Hash(committed[i].content)

Inv == TypeOK /\ AddressMatches /\ ChainIntact /\ NoTornEntry

(***************************************************************************)
(* AppendOnly as an ACTION property: no step may alter or remove an entry  *)
(* that is already committed. This is the temporal statement of the same   *)
(* law the Lean kernel proves structurally as append_preserves_prefix —    *)
(* checked here against every interleaving rather than one append.         *)
(***************************************************************************)
AppendOnly ==
    [][ /\ Len(committed') >= Len(committed)
        /\ \A i \in 1..Len(committed) : committed'[i] = committed[i] ]_vars

=============================================================================
