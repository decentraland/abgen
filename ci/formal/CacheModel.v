(* ========================================================================= *)
(* CacheModel.v — machine-checked model of CI cache correctness for the     *)
(* decentraland/abgen repository (nix + crane layered builds, GitHub CI).   *)
(*                                                                          *)
(* Compile (from the repo root):                                   *)
(*   nix shell nixpkgs#rocq-core --command rocq compile ci/formal/CacheModel.v *)
(*                                                                          *)
(* Theorem map (plain-English claims):                                      *)
(*  design_B_substitution_sound : verified binary cache returns build i for *)
(*      EVERY cache state (corrupt, torn, partial, stale) — fail-open.      *)
(*  stale_hit_safe              : prefix-restore of an old main entry is    *)
(*      still exact (corollary of the above).                               *)
(*  design_B_valid_entry_monotone / _empty_cost / _warm_cost / totals :     *)
(*      cache faults move only the work counter, never the artifact.       *)
(*  design_A_unsound            : unverified snapshot restore has a concrete*)
(*      counterexample cache whose restore <> build i.                      *)
(*  design_A_repaired_sound     : snapshot + nix-store-verify-style repair  *)
(*      is sound again (the f92f891 mitigation).                            *)
(*  deps_layer_sound / deps_reuse_across_src_changes : crane deps layer is  *)
(*      reusable across arbitrary source edits with identical final bytes.  *)
(*  pipeline_factors_through_input + meta_irrelevant + per-field table :    *)
(*      pipeline artifact depends on BuildInput only, never CommitMeta.     *)
(*  buildId_faithful / same_buildId_same_artifact / promote_sound :         *)
(*      equal buildId => equal artifact; tag may reuse main's bytes.        *)
(*  no_false_sharing / key_determines_* : the CI key is complete, so equal  *)
(*      keys never alias distinct semantic inputs (hit-rate property only). *)
(*  design_C_conditionally_sound: rust-cache/cargo-incremental is sound IF  *)
(*      cargo fingerprinting is correct (explicit trust-boundary axiom).    *)
(* ========================================================================= *)

(* ------------------------------------------------------------------------- *)
(* 1. Semantic build inputs vs commit metadata                               *)
(* ------------------------------------------------------------------------- *)

(* Abstract carrier types. The filtered part of the source tree is what the
   nix fileset filter keeps (crate/, Cargo.*, flake.*, lambda/, template/);
   the excluded part is .github/, ci/, *.md — CI and docs. *)
Parameter FilteredSrc ExcludedSrc DepsLock FlakeLock Toolchain Env Arch : Type.

Record SourceTree := mkSrc {
  s_filtered : FilteredSrc ;   (* read by the build *)
  s_excluded : ExcludedSrc     (* .github/, ci/, *.md — never read *)
}.

(* Every input the build derivation can observe. *)
Record BuildInput := mkInput {
  bi_src       : SourceTree ;
  bi_deps      : DepsLock ;    (* Cargo.lock *)
  bi_flake     : FlakeLock ;   (* flake.lock *)
  bi_toolchain : Toolchain ;   (* rust-toolchain.toml *)
  bi_env       : Env ;         (* runner os *)
  bi_arch      : Arch          (* runner arch *)
}.

(* Commit metadata is a SEPARATE record: the build function does not take it.
   Branch/commit/timestamp reach the CI pipeline, never the derivation. *)
Parameter Branch CommitHash Timestamp : Type.

Record CommitMeta := mkMeta {
  cm_branch : Branch ;
  cm_commit : CommitHash ;
  cm_time   : Timestamp
}.

(* ------------------------------------------------------------------------- *)
(* 2. The layered deterministic build                                        *)
(*                                                                           *)
(* MODELING ASSUMPTION (determinism): nix builds are sandboxed and assumed   *)
(* deterministic, so `deps_build` and `final_build` are plain Coq FUNCTIONS. *)
(* Function-ness IS the determinism axiom; no separate Axiom is needed.      *)
(* ------------------------------------------------------------------------- *)

Parameter DepsArtifact Artifact : Type.

(* crane's deps-only derivation: inputs are Cargo.lock + toolchain ONLY. *)
Parameter deps_build : DepsLock -> Toolchain -> DepsArtifact.

(* final derivation: deps artifact + filtered source (+ flake/env/arch). *)
Parameter final_build :
  DepsArtifact -> FilteredSrc -> FlakeLock -> Env -> Arch -> Artifact.

(* The whole build. Note it reads ONLY s_filtered of the source tree: the
   nix fileset filter is realized here BY CONSTRUCTION (stronger than an
   axiom — the definition cannot mention s_excluded or any CommitMeta). *)
Definition build (i : BuildInput) : Artifact :=
  final_build (deps_build (bi_deps i) (bi_toolchain i))
              (s_filtered (bi_src i))
              (bi_flake i) (bi_env i) (bi_arch i).

(* ------------------------------------------------------------------------- *)
(* 3. Hashing                                                                *)
(*                                                                           *)
(* MODELING ASSUMPTION (collision resistance): a cryptographic hash is       *)
(* modeled as an INJECTIVE function. This is the standard idealization: a    *)
(* found collision of sha256 would break the real system exactly where it    *)
(* breaks this model.                                                        *)
(* ------------------------------------------------------------------------- *)

Parameter Hash : Type.

(* Hashes are fixed-width bitstrings, so equality is decidable. *)
Parameter hash_eq_dec : forall h1 h2 : Hash, {h1 = h2} + {h1 <> h2}.

(* Everything the CI ever hashes, as one object universe. *)
Inductive Object : Type :=
  | OArt  : Artifact -> Object
  | ODeps : DepsArtifact -> Object
  | OSrc  : FilteredSrc -> Object.

Parameter hash : Object -> Hash.
Axiom hash_inj : forall x y : Object, hash x = hash y -> x = y.

Definition art_key (a : Artifact) : Hash := hash (OArt a).
Definition deps_key (d : DepsArtifact) : Hash := hash (ODeps d).

Lemma art_key_inj : forall a b : Artifact, art_key a = art_key b -> a = b.
Proof.
  intros a b H. apply hash_inj in H. injection H. intros E. exact E.
Qed.

Lemma deps_key_inj : forall a b : DepsArtifact, deps_key a = deps_key b -> a = b.
Proof.
  intros a b H. apply hash_inj in H. injection H. intros E. exact E.
Qed.

(* ------------------------------------------------------------------------- *)
(* 4. Generic verified cache (Design B), snapshot (Design A), and repair     *)
(* ------------------------------------------------------------------------- *)

Section VerifiedCache.
  Variable Obj : Type.                       (* what the cache stores        *)
  Variable Input : Type.                     (* what a build consumes        *)
  Variable key_of : Obj -> Hash.             (* content address              *)
  Variable build_fn : Input -> Obj.          (* the deterministic build      *)

  (* A cache is ANY partial map Hash -> Obj. Adversarially corrupted, torn,
     partial and stale caches are all just particular values of this type,
     so quantifying over `cache` quantifies over all fault modes. *)
  Definition cache := Hash -> option Obj.
  Definition empty_cache : cache := fun _ => None.

  (* Design B fetch: verify-then-return. A fetched object is used only if
     its content hash equals the requested key; otherwise miss (fail-open). *)
  Definition fetch (c : cache) (k : Hash) : option Obj :=
    match c k with
    | Some o => if hash_eq_dec (key_of o) k then Some o else None
    | None => None
    end.

  Definition wanted (i : Input) : Hash := key_of (build_fn i).

  (* fetch-or-build *)
  Definition realize (c : cache) (i : Input) : Obj :=
    match fetch c (wanted i) with
    | Some o => o
    | None => build_fn i
    end.

  Hypothesis key_of_inj : forall a b : Obj, key_of a = key_of b -> a = b.

  (* HEADLINE (substitution soundness): for EVERY cache state — including
     adversarially corrupted, torn, partial or stale — realize = build. *)
  Theorem realize_sound : forall (c : cache) (i : Input), realize c i = build_fn i.
  Proof.
    intros c i. unfold realize, fetch, wanted.
    destruct (c (key_of (build_fn i))) as [o|] eqn:E.
    - destruct (hash_eq_dec (key_of o) (key_of (build_fn i))) as [He|Hn].
      + apply key_of_inj. exact He.
      + reflexivity.
    - reflexivity.
  Qed.

  (* Work counter: cache faults can only change how many local builds run. *)
  Definition cost (c : cache) (i : Input) : nat :=
    match fetch c (wanted i) with Some _ => 0 | None => 1 end.

  (* Inserting an object at its own content address: a VALID entry. *)
  Definition insert (c : cache) (o : Obj) : cache :=
    fun k => if hash_eq_dec k (key_of o) then Some o else c k.

  Lemma insert_hit : forall c o, fetch (insert c o) (key_of o) = Some o.
  Proof.
    intros c o. unfold fetch, insert.
    destruct (hash_eq_dec (key_of o) (key_of o)) as [E|N].
    - simpl. destruct (hash_eq_dec (key_of o) (key_of o)) as [E2|N2].
      + reflexivity.
      + exfalso. apply N2. reflexivity.
    - exfalso. apply N. reflexivity.
  Qed.

  Lemma le_zero_l : forall n : nat, 0 <= n.
  Proof. induction n. apply le_n. apply le_S. assumption. Qed.

  (* Adding a valid entry never increases the number of builds needed. *)
  Theorem cost_monotone : forall c o i, cost (insert c o) i <= cost c i.
  Proof.
    intros c o i. unfold cost.
    destruct (hash_eq_dec (wanted i) (key_of o)) as [He|Hn].
    - rewrite He. rewrite insert_hit. apply le_zero_l.
    - assert (Ef : fetch (insert c o) (wanted i) = fetch c (wanted i)).
      { unfold fetch, insert.
        destruct (hash_eq_dec (wanted i) (key_of o)) as [He|_].
        - exfalso. apply Hn. exact He.
        - reflexivity. }
      rewrite Ef. apply le_n.
  Qed.

  Theorem empty_cost : forall i, cost empty_cache i = 1.
  Proof. intros i. reflexivity. Qed.

  Theorem warm_cost : forall c i, cost (insert c (build_fn i)) i = 0.
  Proof.
    intros c i. unfold cost, wanted. rewrite insert_hit. reflexivity.
  Qed.

  Fixpoint total_cost (c : cache) (l : list Input) : nat :=
    match l with
    | nil => 0
    | cons i tl => cost c i + total_cost c tl
    end.

  (* Empty cache => every build runs locally... *)
  Theorem empty_total : forall l, total_cost empty_cache l = length l.
  Proof.
    induction l as [|i tl IH].
    - reflexivity.
    - simpl. rewrite IH. reflexivity.
  Qed.

  (* ...a fully warm valid cache => zero builds. In both cases the ARTIFACT
     is `build_fn i` (realize_sound); only the counter differs. *)
  Theorem hot_total :
    forall c l, (forall i : Input, cost c i = 0) -> total_cost c l = 0.
  Proof.
    intros c l H. induction l as [|i tl IH].
    - reflexivity.
    - simpl. rewrite H. rewrite IH. reflexivity.
  Qed.

  (* Design A: snapshot restore, NO verification — trust whatever is there. *)
  Definition snapshot_restore (c : cache) (i : Input) : Obj :=
    match c (wanted i) with
    | Some o => o
    | None => build_fn i
    end.

  (* Repair pass (the f92f891 mitigation, `nix-store --verify`-style): drop
     every entry whose content hash does not match its key. ASSUMPTION,
     stated as this very definition: verify catches EXACTLY the invalid
     registrations — no more, no less. *)
  Definition verify_repair (c : cache) : cache :=
    fun k => match c k with
             | Some o => if hash_eq_dec (key_of o) k then Some o else None
             | None => None
             end.

  (* Snapshot restore becomes sound again after the repair pass. *)
  Theorem verified_snapshot_sound :
    forall c i, snapshot_restore (verify_repair c) i = build_fn i.
  Proof.
    intros c i. unfold snapshot_restore, verify_repair, wanted.
    destruct (c (key_of (build_fn i))) as [o|].
    - destruct (hash_eq_dec (key_of o) (key_of (build_fn i))) as [He|Hn].
      + apply key_of_inj. exact He.
      + reflexivity.
    - reflexivity.
  Qed.

End VerifiedCache.

(* ------------------------------------------------------------------------- *)
(* 5. Design B instantiated for the final artifact                           *)
(* ------------------------------------------------------------------------- *)

Definition ArtCache := cache Artifact.

Definition realizeB : ArtCache -> BuildInput -> Artifact :=
  realize Artifact BuildInput art_key build.

Theorem design_B_substitution_sound :
  forall (c : ArtCache) (i : BuildInput), realizeB c i = build i.
Proof.
  intros c i. unfold realizeB. apply realize_sound. exact art_key_inj.
Qed.

(* Restores happen on ANY branch via key-prefix and may hit a STALE entry
   from an older main. Staleness is just another cache state, so: *)
Theorem stale_hit_safe :
  forall (c_stale : ArtCache) (i : BuildInput), realizeB c_stale i = build i.
Proof. intros c_stale i. apply design_B_substitution_sound. Qed.

Definition costB : ArtCache -> BuildInput -> nat :=
  cost Artifact BuildInput art_key build.

Theorem design_B_valid_entry_monotone :
  forall (c : ArtCache) (o : Artifact) (i : BuildInput),
    costB (insert Artifact art_key c o) i <= costB c i.
Proof. intros c o i. unfold costB. apply cost_monotone. Qed.

Theorem design_B_empty_cost :
  forall i : BuildInput, costB (empty_cache Artifact) i = 1.
Proof. intros i. unfold costB. apply empty_cost. Qed.

Theorem design_B_warm_cost :
  forall (c : ArtCache) (i : BuildInput),
    costB (insert Artifact art_key c (build i)) i = 0.
Proof. intros c i. unfold costB. apply warm_cost. Qed.

Theorem design_B_empty_total :
  forall l : list BuildInput,
    total_cost Artifact BuildInput art_key build (empty_cache Artifact) l
    = length l.
Proof. apply empty_total. Qed.

Theorem design_B_hot_total :
  forall (c : ArtCache) (l : list BuildInput),
    (forall i, costB c i = 0) ->
    total_cost Artifact BuildInput art_key build c l = 0.
Proof. intros c l H. apply hot_total. exact H. Qed.

(* ------------------------------------------------------------------------- *)
(* 6. Design A: the non-theorem, and the repaired variant                    *)
(* ------------------------------------------------------------------------- *)

Definition snapshotA : ArtCache -> BuildInput -> Artifact :=
  snapshot_restore Artifact BuildInput art_key build.

(* MODELING ASSUMPTIONS for the counterexample: some commit exists, and some
   blob (e.g. a torn/truncated restore) is not the output of ANY build. A
   one-artifact universe would make corruption impossible by fiat. *)
Parameter some_input : BuildInput.
Parameter corrupt_blob : Artifact.
Axiom corrupt_blob_not_a_build : forall i : BuildInput, corrupt_blob <> build i.

(* Why the snapshot design cannot be proven sound without verification:
   a concrete cache state whose restore yields an object <> build i. *)
Theorem design_A_unsound :
  exists (c : ArtCache) (i : BuildInput), snapshotA c i <> build i.
Proof.
  exists (fun _ => Some corrupt_blob).
  exists some_input.
  exact (corrupt_blob_not_a_build some_input).
Qed.

(* The f92f891 mitigation: snapshot + verify-as-repair restores soundness. *)
Theorem design_A_repaired_sound :
  forall (c : ArtCache) (i : BuildInput),
    snapshotA (verify_repair Artifact art_key c) i = build i.
Proof.
  intros c i. unfold snapshotA.
  apply verified_snapshot_sound. exact art_key_inj.
Qed.

(* ------------------------------------------------------------------------- *)
(* 7. Layered correctness: the crane deps-only derivation                    *)
(* ------------------------------------------------------------------------- *)

Definition DepsCache := cache DepsArtifact.

Definition deps_build_p (p : DepsLock * Toolchain) : DepsArtifact :=
  deps_build (fst p) (snd p).

Definition realizeD : DepsCache -> DepsLock * Toolchain -> DepsArtifact :=
  realize DepsArtifact (DepsLock * Toolchain) deps_key deps_build_p.

(* The deps-layer cache, keyed by the (Cargo.lock, toolchain) derivation, is
   sound against every cache state, exactly like the final layer. *)
Theorem deps_layer_sound :
  forall (c : DepsCache) (dl : DepsLock) (tc : Toolchain),
    realizeD c (dl, tc) = deps_build dl tc.
Proof.
  intros c dl tc. unfold realizeD.
  apply (realize_sound DepsArtifact (DepsLock * Toolchain)
                       deps_key deps_build_p deps_key_inj).
Qed.

(* Reusing a cached deps artifact across ARBITRARY source changes yields an
   artifact identical to a from-scratch build. *)
Theorem deps_reuse_across_src_changes :
  forall (c : DepsCache) (i : BuildInput),
    final_build (realizeD c (bi_deps i, bi_toolchain i))
                (s_filtered (bi_src i)) (bi_flake i) (bi_env i) (bi_arch i)
    = build i.
Proof.
  intros c i. rewrite deps_layer_sound. reflexivity.
Qed.

(* Two commits agreeing on Cargo.lock + toolchain share the deps artifact,
   no matter how their sources differ. *)
Theorem deps_shared :
  forall i j : BuildInput,
    bi_deps i = bi_deps j -> bi_toolchain i = bi_toolchain j ->
    deps_build (bi_deps i) (bi_toolchain i)
    = deps_build (bi_deps j) (bi_toolchain j).
Proof. intros i j Hd Ht. rewrite Hd. rewrite Ht. reflexivity. Qed.

(* ------------------------------------------------------------------------- *)
(* 8. The CI pipeline: artifact factors through BuildInput alone             *)
(* ------------------------------------------------------------------------- *)

Parameter is_main : Branch -> bool.

(* The pipeline DOES consume CommitMeta — it decides whether to save the
   cache (saves only on main/tags). Branch-irrelevance is therefore not a
   typing triviality: we must prove the ARTIFACT component ignores meta. *)
Record CIResult := mkResult {
  ci_artifact    : Artifact ;
  ci_saved_cache : bool
}.

Definition pipeline (c : ArtCache) (im : BuildInput * CommitMeta) : CIResult :=
  mkResult (realizeB c (fst im)) (is_main (cm_branch (snd im))).

Theorem pipeline_factors_through_input :
  forall (c : ArtCache) (i : BuildInput) (m : CommitMeta),
    ci_artifact (pipeline c (i, m)) = build i.
Proof. intros c i m. apply design_B_substitution_sound. Qed.

Theorem meta_irrelevant :
  forall (c : ArtCache) (i : BuildInput) (m m' : CommitMeta),
    ci_artifact (pipeline c (i, m)) = ci_artifact (pipeline c (i, m')).
Proof.
  intros c i m m'.
  rewrite (pipeline_factors_through_input c i m).
  rewrite (pipeline_factors_through_input c i m').
  reflexivity.
Qed.

(* Per-aspect table for the deliberately-EXCLUDED metadata: changing only
   the branch, only the commit hash, or only the timestamp never changes
   the produced artifact. *)
Theorem branch_change_irrelevant :
  forall c i b1 b2 h t,
    ci_artifact (pipeline c (i, mkMeta b1 h t))
    = ci_artifact (pipeline c (i, mkMeta b2 h t)).
Proof. intros. apply meta_irrelevant. Qed.

Theorem commit_hash_change_irrelevant :
  forall c i b h1 h2 t,
    ci_artifact (pipeline c (i, mkMeta b h1 t))
    = ci_artifact (pipeline c (i, mkMeta b h2 t)).
Proof. intros. apply meta_irrelevant. Qed.

Theorem timestamp_change_irrelevant :
  forall c i b h t1 t2,
    ci_artifact (pipeline c (i, mkMeta b h t1))
    = ci_artifact (pipeline c (i, mkMeta b h t2)).
Proof. intros. apply meta_irrelevant. Qed.

(* ------------------------------------------------------------------------- *)
(* 9. buildId: hash of the filtered source tree                              *)
(* ------------------------------------------------------------------------- *)

Definition buildId (s : SourceTree) : Hash := hash (OSrc (s_filtered s)).

Theorem buildId_faithful :
  forall s s' : SourceTree, buildId s = buildId s' -> s_filtered s = s_filtered s'.
Proof.
  intros s s' H. apply hash_inj in H. injection H. intros E. exact E.
Qed.

(* Edits confined to .github/, ci/, *.md preserve the buildId... *)
Theorem excluded_edit_preserves_buildId :
  forall (f : FilteredSrc) (e e' : ExcludedSrc),
    buildId (mkSrc f e) = buildId (mkSrc f e').
Proof. intros f e e'. reflexivity. Qed.

Definition with_excluded (i : BuildInput) (e : ExcludedSrc) : BuildInput :=
  mkInput (mkSrc (s_filtered (bi_src i)) e)
          (bi_deps i) (bi_flake i) (bi_toolchain i) (bi_env i) (bi_arch i).

(* ...and the artifact (definitional: build never reads s_excluded). *)
Theorem excluded_edit_preserves_artifact :
  forall (i : BuildInput) (e : ExcludedSrc), build (with_excluded i e) = build i.
Proof. intros i e. reflexivity. Qed.

(* Equal buildId => equal artifact (given the remaining pinned aspects; in
   the real CI, Cargo.lock and flake.lock live INSIDE the filtered tree and
   toolchain/os/arch are pinned by the runner, so these hypotheses are
   discharged for free there). *)
Theorem same_buildId_same_artifact :
  forall i j : BuildInput,
    buildId (bi_src i) = buildId (bi_src j) ->
    bi_deps i = bi_deps j -> bi_flake i = bi_flake j ->
    bi_toolchain i = bi_toolchain j ->
    bi_env i = bi_env j -> bi_arch i = bi_arch j ->
    build i = build j.
Proof.
  intros i j Hb Hd Hf Ht He Ha.
  apply buildId_faithful in Hb.
  unfold build. rewrite Hb. rewrite Hd. rewrite Hf. rewrite Ht.
  rewrite He. rewrite Ha. reflexivity.
Qed.

(* Promote-on-tag: a tag whose buildId matches a main-built artifact may
   reuse main's bytes verbatim. *)
Theorem promote_sound :
  forall (main_i tag_i : BuildInput),
    buildId (bi_src main_i) = buildId (bi_src tag_i) ->
    bi_deps main_i = bi_deps tag_i -> bi_flake main_i = bi_flake tag_i ->
    bi_toolchain main_i = bi_toolchain tag_i ->
    bi_env main_i = bi_env tag_i -> bi_arch main_i = bi_arch tag_i ->
    build tag_i = build main_i.
Proof.
  intros main_i tag_i Hb Hd Hf Ht He Ha.
  symmetry. apply same_buildId_same_artifact; assumption.
Qed.

(* ------------------------------------------------------------------------- *)
(* 10. Cache-key completeness (a hit-rate property, NOT needed for safety)   *)
(* ------------------------------------------------------------------------- *)

Parameter CacheKey : Type.

(* The CI key = hashFiles(flake.lock, Cargo.lock, **/Cargo.toml,
   rust-toolchain.toml, nix/*.nix) + runner os/arch. Its domain is
   BuildInput ONLY: branch, commit hash and timestamp cannot influence it
   by typing (there is no CommitMeta argument to influence it with). *)
Parameter ci_key : BuildInput -> CacheKey.

(* MODELING ASSUMPTION (key completeness): the hashFiles list covers every
   semantic input, so equal keys mean equal inputs — no false sharing.
   NOTE: Design B does NOT need this for correctness (theorem
   design_B_substitution_sound holds for every cache); completeness only
   protects the HIT RATE from aliasing two different inputs to one slot. *)
Axiom key_complete : forall i j : BuildInput, ci_key i = ci_key j -> i = j.

Theorem no_false_sharing :
  forall i j : BuildInput, ci_key i = ci_key j -> build i = build j.
Proof. intros i j H. apply key_complete in H. rewrite H. reflexivity. Qed.

(* Per-aspect table: the key pins every included semantic aspect. *)
Theorem key_determines_src :
  forall i j, ci_key i = ci_key j -> bi_src i = bi_src j.
Proof. intros i j H. apply key_complete in H. rewrite H. reflexivity. Qed.

Theorem key_determines_deps :
  forall i j, ci_key i = ci_key j -> bi_deps i = bi_deps j.
Proof. intros i j H. apply key_complete in H. rewrite H. reflexivity. Qed.

Theorem key_determines_flake :
  forall i j, ci_key i = ci_key j -> bi_flake i = bi_flake j.
Proof. intros i j H. apply key_complete in H. rewrite H. reflexivity. Qed.

Theorem key_determines_toolchain :
  forall i j, ci_key i = ci_key j -> bi_toolchain i = bi_toolchain j.
Proof. intros i j H. apply key_complete in H. rewrite H. reflexivity. Qed.

Theorem key_determines_env :
  forall i j, ci_key i = ci_key j -> bi_env i = bi_env j.
Proof. intros i j H. apply key_complete in H. rewrite H. reflexivity. Qed.

Theorem key_determines_arch :
  forall i j, ci_key i = ci_key j -> bi_arch i = bi_arch j.
Proof. intros i j H. apply key_complete in H. rewrite H. reflexivity. Qed.

(* ------------------------------------------------------------------------- *)
(* 11. Design C: rust-cache / cargo incremental — an unverified mutable hint *)
(* ------------------------------------------------------------------------- *)

Parameter CargoHint : Type.                      (* target/ dir contents *)
Parameter compile      : FilteredSrc -> Artifact.
Parameter compile_with : CargoHint -> FilteredSrc -> Artifact.

(* ======================= TRUST BOUNDARY — READ ME ======================= *)
(* This axiom IS the entire safety argument for rust-cache and cargo        *)
(* incremental compilation. Cargo's fingerprinting (mtime + metadata based) *)
(* is TRUSTED, not verified: nothing checks the restored target/ dir        *)
(* against a content hash. If cargo ever consults a stale hint it should    *)
(* have rejected, this axiom is FALSE and design C is unsound — unlike      *)
(* design B, which needs no such trust.                                     *)
(* ======================================================================== *)
Axiom cargo_fingerprint_correct :
  forall (h : CargoHint) (s : FilteredSrc), compile_with h s = compile s.

Definition rust_cache_realize (h : CargoHint) (s : FilteredSrc) : Artifact :=
  compile_with h s.

(* Conditional soundness: sound exactly as far as the axiom above holds. *)
Theorem design_C_conditionally_sound :
  forall (h : CargoHint) (s : FilteredSrc), rust_cache_realize h s = compile s.
Proof. intros h s. apply cargo_fingerprint_correct. Qed.

Theorem design_C_hint_state_irrelevant :
  forall (h h' : CargoHint) (s : FilteredSrc),
    rust_cache_realize h s = rust_cache_realize h' s.
Proof.
  intros h h' s. unfold rust_cache_realize.
  rewrite (cargo_fingerprint_correct h s).
  rewrite (cargo_fingerprint_correct h' s).
  reflexivity.
Qed.

(* ========================================================================= *)
(* End of model. Axioms used (beyond the abstract-type signature):           *)
(*   hash_inj, corrupt_blob_not_a_build, key_complete,                       *)
(*   cargo_fingerprint_correct                                               *)
(* — each is an explicitly stated modeling assumption above.                 *)
(* ========================================================================= *)
