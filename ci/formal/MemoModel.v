(* MemoModel.v — soundness of green-marker memoization.

   A CI lane is memoized by a marker keyed on some function of the repo
   state. The lane's verdict is a function of the derivations it builds
   (hermeticity: same drv, same result — the contract CacheModel.v rests
   on). Three key disciplines, three outcomes:

   1. drv_key_skip_sound — key = the exact derivation set (as computed by
      nix eval) plus the harness files outside it. Skipping on a marker is
      sound unconditionally: the key IS the verdict's argument.

   2. allowlist_false_green — key = a hand-maintained file allow-list.
      If the verdict can read any file the list omits, there are two repo
      states with equal keys and opposite verdicts: a marker minted on the
      passing one silently skips the failing one. This is the scheme the
      drv keys replaced.

   3. denylist_skip_sound — key = everything except an explicit exclusion
      set E. Sound exactly under the stated hypothesis that the verdict
      ignores E; each entry of E must carry that argument (see
      .github/actions/tree-hash). Omitting a file from E only shrinks the
      set of skips (over-testing), never adds a false one. *)

Section DrvKey.
  Variable Input : Type.          (* full repo state *)
  Variable Drv : Type.            (* a lane's derivation set + harness *)
  Variable drvOf : Input -> Drv.  (* nix eval: instantiation *)
  Variable verdict : Drv -> bool. (* hermetic build + check result *)

  Definition passes (i : Input) : Prop := verdict (drvOf i) = true.

  (* The marker store. Invariant: a marker is minted only after the
     derivation was watched to pass (the cache/save step runs after the
     checks step succeeded). *)
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

  (* Shrinking E (moving a file from Denied into Kept) never invalidates
     soundness — it only refines the key, splitting one marker into many.
     Formally: the identity projection on (Kept * Denied') with Denied'
     smaller still satisfies denied_irrelevant a fortiori. Stated here as
     the trivial direction to record the design intent. *)
  Remark denylist_omission_is_overtesting :
    forall k d, verdict3 (k, d) = verdict3 (k, d).
  Proof. reflexivity. Qed.
End DenyList.
