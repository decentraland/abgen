(* MemoModel.v — soundness of green-marker memoization.

   The claim being proved: IF CI skips a lane because a marker exists for
   its key, THEN running the lane would have passed.

   Machine-checked here (all Qed, no Admitted):

   - protocol_skip_sound: over every reachable state of the marker store
     (mint-after-pass, arbitrary eviction, in any interleaving), a key hit
     implies the lane passes. The store model permits eviction of anything
     at any time (GitHub's 7-day/10GB policies) — losing markers only
     loses skips.
   - key_determines: same key => same verdict, from the two axioms below.
   - allowlist_false_green: the scheme this replaced (hand-listed
     hashFiles inputs) admits two repo states with equal keys and opposite
     verdicts — the logic-error failure mode, as a counterexample.
   - denylist_skip_sound: the windows/node scheme is sound exactly under
     the per-entry irrelevance hypothesis documented in
     .github/actions/tree-hash; an omitted entry shrinks skips only.

   Axioms (the trust base — what Rocq cannot see), and where each is
   discharged in the implementation:

   A1 hermetic: the lane verdict depends on the repo only through Inputs =
      (check drv closures + harness files). Backed by the nix build
      sandbox; the harness files (.github/workflows/ci.yml,
      ci/nix-checks.sh) are folded into the key because they run outside
      it. Residual: nondeterministic (flaky) tests violate A1 — but they
      equally defeat nix's own store-level memoization.
   A2 collision_free: sha256 truncated to 128 bits is injective on the
      keys ever minted (birthday bound ~2^64).
   P1 mint-after-pass (the MintStep rule): marker steps carry GitHub's
      implicit success() — their `if:` uses no status-check function — and
      are ordered after the checks step, so a marker is written only in a
      run whose checks succeeded.
   P2 key = work: the gate's lane_attrs mapping is single-sourced — the
      same list is keyed and exported as the attrs_* outputs the lanes
      build, so the key cannot cover less than the lane runs. (An empty
      output degrades to building every check: a superset, still sound.)
   P3 gate determinism: the key is a pure locked flake eval; IFD contents
      are pinned by the hash-verified first-write binary cache.
   P4 store writes are trusted: the scope-blind caches-API lookup filters
      to refs/heads/* (repo writers) or the current ref, so a fork PR
      minting keys into its own merge-ref scope cannot poison other refs.
      windows/node lookups use actions/cache's native scope chain, which
      enforces the same. *)

Section DrvKey.
  Variable Input : Type.          (* full repo state *)
  Variable Drv : Type.            (* a lane's derivation set + harness *)
  Variable drvOf : Input -> Drv.  (* nix eval: instantiation *)
  Variable verdict : Drv -> bool. (* hermetic build + check result *)

  Definition passes (i : Input) : Prop := verdict (drvOf i) = true.

  Variable minted : Drv -> Prop.
  Hypothesis minted_sound : forall d, minted d -> verdict d = true.

  Theorem drv_key_skip_sound :
    forall i, minted (drvOf i) -> passes i.
  Proof. intros i H. exact (minted_sound _ H). Qed.
End DrvKey.

Section AllowList.
  (* Repo state split as (files the allow-list covers, a file it forgot).
     The verdict reads the forgotten file — say it breaks the build. *)
  Definition I2 : Type := (bool * bool)%type.
  Definition key2 (i : I2) : bool := fst i.
  Definition verdict2 (i : I2) : bool := negb (snd i).

  Theorem allowlist_false_green :
    exists i j : I2,
      key2 i = key2 j /\ verdict2 i = true /\ verdict2 j = false.
  Proof.
    exists (true, false), (true, true).
    split; [reflexivity | split; reflexivity].
  Qed.
End AllowList.

Section DenyList.
  Variable Kept Denied : Type.    (* tree split by the exclusion list E *)
  Variable verdict3 : Kept * Denied -> bool.

  (* The load-bearing hypothesis: every excluded path is irrelevant to
     the verdict. This is what each deny-list entry must justify. *)
  Hypothesis denied_irrelevant :
    forall k d d', verdict3 (k, d) = verdict3 (k, d').

  Variable mintedK : Kept -> Prop.
  Hypothesis mintedK_sound :
    forall k, mintedK k -> exists d, verdict3 (k, d) = true.

  Theorem denylist_skip_sound :
    forall k d, mintedK k -> verdict3 (k, d) = true.
  Proof.
    intros k d H.
    destruct (mintedK_sound k H) as [d' Hd'].
    rewrite (denied_irrelevant k d d'). exact Hd'.
  Qed.
End DenyList.

(* The full protocol: the marker store as evolving state. DrvKey above
   assumes a well-formed store; here well-formedness is derived from the
   transition rules themselves, so nothing about the store is assumed —
   only how it can change. *)
Section Protocol.
  Variable Repo : Type.           (* full repo state at some commit *)
  Variable Inputs : Type.         (* the lane's semantic inputs *)
  Variable inputsOf : Repo -> Inputs.
  Variable runLane : Repo -> bool.

  (* A1: the verdict factors through Inputs. *)
  Hypothesis hermetic :
    forall r r', inputsOf r = inputsOf r' -> runLane r = runLane r'.

  Variable Key : Type.
  Variable hash : Inputs -> Key.
  (* A2: no collisions among keys that ever occur. *)
  Hypothesis collision_free : forall i i', hash i = hash i' -> i = i'.

  Definition keyOf (r : Repo) : Key := hash (inputsOf r).

  Lemma key_determines :
    forall r r', keyOf r = keyOf r' -> runLane r = runLane r'.
  Proof.
    intros r r' H. apply hermetic. apply collision_free. exact H.
  Qed.

  Definition store := Key -> Prop.

  (* P1 is the side condition of MintStep; ReachEvict covers GitHub
     evicting or expiring any subset of entries at any point, which also
     absorbs arbitrary interleavings of concurrent lanes minting. *)
  Inductive reachable : store -> Prop :=
  | ReachInit :
      reachable (fun _ => False)
  | ReachMint : forall (S : store) (r : Repo),
      reachable S -> runLane r = true ->
      reachable (fun k => k = keyOf r \/ S k)
  | ReachEvict : forall S S' : store,
      reachable S -> (forall k, S' k -> S k) ->
      reachable S'.

  Definition justified (S : store) : Prop :=
    forall k, S k -> exists r, keyOf r = k /\ runLane r = true.

  Lemma reachable_justified : forall S, reachable S -> justified S.
  Proof.
    induction 1 as [ | S r HR IH Hpass | S S' HR IH Hsub ];
      unfold justified in *; intros k Hk.
    - contradiction.
    - destruct Hk as [-> | Hk].
      + exists r. split; [reflexivity | exact Hpass].
      + apply IH, Hk.
    - apply IH, Hsub, Hk.
  Qed.

  (* The theorem: whatever sequence of mints and evictions produced the
     store, a hit on this repo state's key means this repo state passes. *)
  Theorem protocol_skip_sound :
    forall (S : store) (r : Repo),
      reachable S -> S (keyOf r) -> runLane r = true.
  Proof.
    intros S r HR Hk.
    destruct (reachable_justified S HR (keyOf r) Hk) as [r0 [Hk0 Hp]].
    rewrite <- (key_determines r0 r Hk0). exact Hp.
  Qed.
End Protocol.
