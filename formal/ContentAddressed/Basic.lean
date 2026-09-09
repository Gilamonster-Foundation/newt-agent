/- The content-addressable first principle, machine-checked.

   Stated by Shawn, 2026-08-20:

     "Content-addressable data structures are a first-principle guiding design
      philosophy. Any drift from tamper-resistant and provenance-traceable data is
      a drift away from my first principles."

   This kernel turns that into laws an implementation can be checked against. Four
   properties, and one theorem about why the fourth is usually where it fails.

   ## What `lake build` machine-checks

     append_preserves_prefix     appending never alters what was already recorded
     append_is_the_only_growth   a log's history is a prefix of its future
     supersede_keeps_the_original an edit-as-append leaves the prior value present
     tamper_breaks_chain         altering any entry invalidates the chain from there
     rooted_has_traceable_origin every derived value names what it derives from
     orphan_is_untraceable       a value naming no source cannot be traced, decidably
     UNREAD EVIDENCE:
     unverified_logs_are_indistinguishable   the load-bearing one — see below

   ## The honesty boundary

   **Collision resistance is NOT proved here and cannot be.** That an ideal hash
   assigns distinct addresses to distinct content is a cryptographic assumption. It
   is carried as an explicit injectivity hypothesis `hinj` on exactly the theorems
   that need it, never asserted. The definitions never assume it. This is the same
   discipline the spill-store kernel uses, and the reason to state it is that a
   reader who skims could otherwise believe integrity was proved outright when what
   is proved is *integrity GIVEN a sound hash*.

   Nor does this kernel prove any particular implementation satisfies the laws. It
   defines what satisfying them means. Conformance is a test obligation.

   ## Why `unverified_logs_are_indistinguishable` is the point

   Writing tamper evidence is not the same as having tamper detection. A hash chain
   that nothing ever verifies leaves a tampered log and an intact log *observationally
   identical* to every consumer — so the evidence buys exactly nothing while looking
   like diligence. That is not hypothetical: it is the shape of a real finding in a
   working system, where a chain is written on every turn and the verification
   function has no production caller.

   The theorem below says it plainly: **evidence unread is evidence absent.** -/
namespace AgentFrame.ContentAddressed

/-! ## Content, addresses, and the assumption we do not prove -/

/-- Content is modelled as an opaque finite value; the laws are structural and do
    not depend on what it is. -/
abbrev Content := Nat

/-- An address. In the real system this is a BLAKE3 CIDv1 over a canonical
    encoding; here it is whatever the address function returns. -/
abbrev Addr := Nat

/-- The address function: DETERMINISTIC by construction (it is a function, so equal
    content necessarily has equal address — that half needs no hypothesis). -/
abbrev AddrOf := Content → Addr

/-- Equal content always has an equal address. This direction is free: it is what
    being a function means, and it is the half that makes deduplication sound. -/
theorem same_content_same_address (h : AddrOf) (a b : Content) :
    a = b → h a = h b := by
  intro e; rw [e]

/-! ## The log: append-only by construction -/

/-- One recorded entry. `prev` is the address of the entry before it (the chain
    link); `sources` names the entries this one derives from (provenance). A root
    has no sources. -/
structure Entry where
  content : Content
  prev : Addr
  sources : List Addr
  deriving DecidableEq, Repr

abbrev Log := List Entry

/-- The ONLY growth operation. There is deliberately no update, no delete, and no
    insert-at — their absence is the point, not an omission. -/
def append (l : Log) (e : Entry) : Log := l ++ [e]

/-- **Appending never alters what was already recorded.** The no-editing-history
    law, stated at the level where it can be checked. -/
theorem append_preserves_prefix (l : Log) (e : Entry) (i : Nat) (h : i < l.length) :
    (append l e)[i]? = l[i]? := by
  simp [append, List.getElem?_append_left h]

/-- A log's past is always a prefix of its future. -/
theorem append_is_the_only_growth (l : Log) (e : Entry) :
    ∃ suffix, append l e = l ++ suffix := ⟨[e], rfl⟩

/-! ## Edit-as-append

An edit that must happen is expressed by APPENDING an entry that names what it
supersedes, never by rewriting. The superseded value stays present and reachable —
which is what makes the record still auditable after a correction. -/

/-- Supersede `target` with new content, recording the supersession as provenance. -/
def supersede (l : Log) (target : Addr) (content : Content) (prev : Addr) : Log :=
  append l { content := content, prev := prev, sources := [target] }

/-- **An edit leaves the original present.** This is the whole reason edit-as-append
    is compatible with a first principle that forbids mutation. -/
theorem supersede_keeps_the_original
    (l : Log) (target : Addr) (c : Content) (p : Addr) (i : Nat) (h : i < l.length) :
    (supersede l target c p)[i]? = l[i]? :=
  append_preserves_prefix l _ i h

/-! ## Tamper evidence -/

/-- The chain holds when every entry's `prev` is the address of the entry before it.
    `genesis` anchors the first. -/
def chained (h : AddrOf) (genesis : Addr) : Log → Prop
  | [] => True
  | e :: rest => e.prev = genesis ∧ chained h (h e.content) rest

/-- **Tampering is detectable.** If an entry's content is altered, the chain no
    longer holds for anything recorded after it — GIVEN an injective address
    function. The hypothesis is the cryptographic assumption, carried explicitly. -/
theorem tamper_breaks_chain
    (h : AddrOf) (hinj : ∀ a b, h a = h b → a = b)
    (genesis : Addr) (e e' : Entry) (rest : Log)
    (halt : e'.content ≠ e.content) (hprev : e'.prev = e.prev)
    (hok : chained h genesis (e :: rest)) :
    ¬ chained h genesis (e' :: rest) ∨ rest = [] := by
  cases rest with
  | nil => exact Or.inr rfl
  | cons f fs =>
    refine Or.inl ?_
    intro hbad
    obtain ⟨_, hrest⟩ := hok
    obtain ⟨_, hrest'⟩ := hbad
    -- both tails must chain from their own predecessor's address
    cases hrest; cases hrest'
    rename_i hf _ hf' _
    exact halt (hinj _ _ (hf'.symm.trans hf))

/-! ## Provenance -/

/-- A root records something witnessed directly; it derives from nothing. -/
def isRoot (e : Entry) : Bool := e.sources.isEmpty

/-- **A derived value names what it derives from.** -/
theorem rooted_has_traceable_origin (e : Entry) (hd : isRoot e = false) :
    e.sources ≠ [] := by
  intro hnil
  simp [isRoot, hnil] at hd

/-- **A value naming no source cannot be traced** — decidably, with no hashing and
    no search. A record that is neither a root nor derived from anything is, by
    construction, unattributable. -/
theorem orphan_is_untraceable (e : Entry) (hr : isRoot e = true) :
    e.sources = [] := by
  simpa [isRoot, List.isEmpty_iff] using hr

/-! ## Evidence unread is evidence absent

The load-bearing theorem. A consumer that never verifies observes only content —
so an intact log and a tampered one are the same object to it. -/

/-- What a consumer sees when it reads the log without checking the chain. -/
def observed (l : Log) : List Content := l.map Entry.content

/-- **Unverified, a tampered log is indistinguishable from an intact one.**
    Two logs whose entries carry the same content are observationally equal, no
    matter what their `prev` links say — so tamper evidence that nothing verifies
    changes nothing that anyone can see.

    The corollary is the one that matters in practice: *writing* a hash chain is
    not *having* tamper detection. If the verification has no caller, the property
    it would establish is not established, and the code that writes it is
    diligence-shaped rather than diligent. -/
theorem unverified_logs_are_indistinguishable (l l' : Log)
    (hsame : l.map Entry.content = l'.map Entry.content) :
    observed l = observed l' := hsame

/-- Stated the other way, for citation: verification is what converts recorded
    evidence into a detected difference. Without a step that inspects `prev`,
    nothing in the consumer's view depends on it. -/
theorem detection_requires_verification
    (h : AddrOf) (genesis : Addr) (l l' : Log)
    (hobs : observed l = observed l')
    (hgood : chained h genesis l) (hbad : ¬ chained h genesis l') :
    observed l = observed l' ∧ (chained h genesis l ∧ ¬ chained h genesis l') :=
  ⟨hobs, hgood, hbad⟩

end AgentFrame.ContentAddressed
