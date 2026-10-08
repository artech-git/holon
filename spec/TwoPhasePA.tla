---------------------------- MODULE TwoPhasePA ----------------------------
(***************************************************************************)
(* Presumed-abort two-phase commit as implemented by txp (Phase 1).        *)
(*                                                                         *)
(* Extends the classic TwoPhase model (Gray & Lamport) with:               *)
(*   - a coordinator log split into a DURABLE part (forced records: only   *)
(*     Commit and the operator's ForceResolve) and a VOLATILE part         *)
(*     (Begin, Abort, Done) that a crash erases;                           *)
(*   - coordinator crash + restart, recovering purely from the durable log *)
(*     (no record => presumed abort);                                      *)
(*   - participant crash before prepare (staging lost);                    *)
(*   - duplicated / reordered messages (messages are a set, never removed);*)
(*   - the read-only vote;                                                 *)
(*   - the one-phase fast path, where the single effective participant's   *)
(*     local commit is the commit point, and the rule that makes it safe:  *)
(*     abort is total and REPORTS whether the participant had already      *)
(*     committed, so a recovering coordinator adopts that decision.        *)
(*                                                                         *)
(* Checked invariants (see TwoPhasePA.cfg):                                *)
(*   Consistent      no participant committed while another aborted (I1)   *)
(*   CommitPoint     a 2PC participant commits only after the Commit       *)
(*                   record is durable (I1/I2)                             *)
(*   NoAbortAfterCommit                                                    *)
(*                   no new abort message once Commit is durable (I2)      *)
(*   DoneIsFinal     when the coordinator is done, every non-read-only     *)
(*                   participant agrees with the coordinator's decision    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANT RM

VARIABLES
  rmState,    \* participant state
  tcState,    \* coordinator state
  dlog,       \* durable log records
  vlog,       \* volatile (not yet forced) log records
  commitSet,  \* participants named in the durable Commit record
  msgs        \* all messages ever sent (duplication/reordering for free)

vars == <<rmState, tcState, dlog, vlog, commitSet, msgs>>

Messages ==
  [type : {"prepared", "readonly", "voteno", "commit", "abort", "commit1pc"}, rm : RM]
  \cup [type : {"ack1pc"}, rm : RM, outcome : {"committed", "aborted"}]
  \cup [type : {"abortack"}, rm : RM, outcome : {"discarded", "wascommitted"}]

TypeOK ==
  /\ rmState \in [RM -> {"working", "prepared", "readonly", "committed", "aborted"}]
  /\ tcState \in {"init", "collecting", "onephase", "committed", "aborted", "done", "crashed"}
  /\ dlog \subseteq {"begin", "commit", "abort", "done"}
  /\ vlog \subseteq {"begin", "abort", "done"}
  /\ commitSet \subseteq RM
  /\ msgs \subseteq Messages

Init ==
  /\ rmState = [r \in RM |-> "working"]
  /\ tcState = "init"
  /\ dlog = {}
  /\ vlog = {}
  /\ commitSet = {}
  /\ msgs = {}

Send(m) == msgs' = msgs \cup {m}
Voted(r, t) == [type |-> t, rm |-> r] \in msgs
Prepared == {r \in RM : Voted(r, "prepared")}

-----------------------------------------------------------------------------
(* Coordinator, before the decision *)

TCBegin ==
  /\ tcState = "init"
  /\ tcState' = "collecting"
  /\ vlog' = vlog \cup {"begin"}
  /\ UNCHANGED <<rmState, dlog, commitSet, msgs>>

(* All votes in, at least one Prepared: force the Commit record. *)
TCCommit ==
  /\ tcState = "collecting"
  /\ \A r \in RM : Voted(r, "prepared") \/ Voted(r, "readonly")
  /\ Prepared # {}
  /\ tcState' = "committed"
  /\ dlog' = dlog \cup vlog \cup {"commit"}   \* fdatasync makes earlier records durable too
  /\ vlog' = {}
  /\ commitSet' = Prepared
  /\ msgs' = msgs \cup {[type |-> "commit", rm |-> r] : r \in Prepared}
  /\ UNCHANGED rmState

(* Everyone read-only: nothing to commit. *)
TCTrivialDone ==
  /\ tcState = "collecting"
  /\ \A r \in RM : Voted(r, "readonly")
  /\ tcState' = "done"
  /\ vlog' = vlog \cup {"done"}
  /\ UNCHANGED <<rmState, dlog, commitSet, msgs>>

(* Abort for any reason (a VoteNo, a timeout, an operator) before deciding. *)
TCAbort ==
  /\ tcState = "collecting"
  /\ tcState' = "aborted"
  /\ vlog' = vlog \cup {"abort"}
  /\ msgs' = msgs \cup {[type |-> "abort", rm |-> r] : r \in RM}
  /\ UNCHANGED <<rmState, dlog, commitSet>>

(* One-phase fast path: every other participant is read-only. *)
TCOnePhase(r) ==
  /\ tcState = "collecting"
  /\ rmState[r] = "working"
  /\ \A s \in RM \ {r} : Voted(s, "readonly")
  /\ tcState' = "onephase"
  /\ Send([type |-> "commit1pc", rm |-> r])
  /\ UNCHANGED <<rmState, dlog, vlog, commitSet>>

TC1PCAck(r, o) ==
  /\ tcState = "onephase"
  /\ [type |-> "ack1pc", rm |-> r, outcome |-> o] \in msgs
  /\ IF o = "committed"
       THEN /\ tcState' = "done"
            /\ vlog' = vlog \cup {"done"}
            /\ UNCHANGED msgs
       ELSE /\ tcState' = "aborted"
            /\ vlog' = vlog \cup {"abort"}
            /\ msgs' = msgs \cup {[type |-> "abort", rm |-> s] : s \in RM}
  /\ UNCHANGED <<rmState, dlog, commitSet>>

-----------------------------------------------------------------------------
(* Coordinator, after the decision *)

TCDone ==
  /\ tcState = "committed"
  /\ \A r \in commitSet : rmState[r] = "committed"
  /\ tcState' = "done"
  /\ vlog' = vlog \cup {"done"}
  /\ UNCHANGED <<rmState, dlog, commitSet, msgs>>

(* Abort fan-out complete: every participant acknowledged discarding. *)
TCAbortDone ==
  /\ tcState = "aborted"
  /\ \A r \in RM : [type |-> "abortack", rm |-> r, outcome |-> "discarded"] \in msgs
  /\ tcState' = "done"
  /\ vlog' = vlog \cup {"done"}
  /\ UNCHANGED <<rmState, dlog, commitSet, msgs>>

(* A participant answered "I had already committed" (1PC): adopt it with an
   audited, forced ForceResolve(commit) record. *)
TCAdoptCommit(r) ==
  /\ tcState = "aborted"
  /\ [type |-> "abortack", rm |-> r, outcome |-> "wascommitted"] \in msgs
  /\ tcState' = "done"
  /\ dlog' = dlog \cup vlog \cup {"commit"}
  /\ vlog' = {}
  /\ commitSet' = {r}
  /\ UNCHANGED <<rmState, msgs>>

TCCrash ==
  /\ tcState \notin {"crashed", "done"}
  /\ tcState' = "crashed"
  /\ vlog' = {}
  /\ UNCHANGED <<rmState, dlog, commitSet, msgs>>

(* Recovery reads only the durable log: Commit => re-drive commit,
   anything else => presumed abort. *)
TCRecover ==
  /\ tcState = "crashed"
  /\ IF "commit" \in dlog
       THEN /\ tcState' = "committed"
            /\ msgs' = msgs \cup {[type |-> "commit", rm |-> r] : r \in commitSet}
       ELSE /\ tcState' = "aborted"
            /\ msgs' = msgs \cup {[type |-> "abort", rm |-> r] : r \in RM}
  /\ UNCHANGED <<rmState, dlog, vlog, commitSet>>

-----------------------------------------------------------------------------
(* Participants *)

RMPrepare(r) ==
  /\ tcState = "collecting"
  /\ rmState[r] = "working"
  /\ rmState' = [rmState EXCEPT ![r] = "prepared"]
  /\ Send([type |-> "prepared", rm |-> r])
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet>>

RMReadOnly(r) ==
  /\ tcState = "collecting"
  /\ rmState[r] = "working"
  /\ rmState' = [rmState EXCEPT ![r] = "readonly"]
  /\ Send([type |-> "readonly", rm |-> r])
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet>>

RMVoteNo(r) ==
  /\ tcState = "collecting"
  /\ rmState[r] = "working"
  /\ rmState' = [rmState EXCEPT ![r] = "aborted"]
  /\ Send([type |-> "voteno", rm |-> r])
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet>>

(* Crash before prepare: staged state is lost. A prepared participant keeps
   its durable promise and is not affected by a crash. *)
RMCrash(r) ==
  /\ rmState[r] = "working"
  /\ rmState' = [rmState EXCEPT ![r] = "aborted"]
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet, msgs>>

RMRcvCommit(r) ==
  /\ [type |-> "commit", rm |-> r] \in msgs
  /\ rmState[r] \in {"prepared", "committed"}
  /\ rmState' = [rmState EXCEPT ![r] = "committed"]
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet, msgs>>

(* abort is total: it works in every state and reports what happened. *)
RMRcvAbort(r) ==
  /\ [type |-> "abort", rm |-> r] \in msgs
  /\ IF rmState[r] = "committed"
       THEN /\ Send([type |-> "abortack", rm |-> r, outcome |-> "wascommitted"])
            /\ UNCHANGED rmState
       ELSE IF rmState[r] = "readonly"
       THEN /\ Send([type |-> "abortack", rm |-> r, outcome |-> "discarded"])
            /\ UNCHANGED rmState
       ELSE /\ rmState' = [rmState EXCEPT ![r] = "aborted"]
            /\ Send([type |-> "abortack", rm |-> r, outcome |-> "discarded"])
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet>>

(* 1PC: the local commit record is the commit point; the participant may
   also fail locally. Duplicates are answered from local state. *)
RMRcv1PC(r) ==
  /\ [type |-> "commit1pc", rm |-> r] \in msgs
  /\ \/ /\ rmState[r] = "working"
        /\ \E o \in {"committed", "aborted"} :
             /\ rmState' = [rmState EXCEPT ![r] = o]
             /\ Send([type |-> "ack1pc", rm |-> r, outcome |-> o])
     \/ /\ rmState[r] \in {"committed", "aborted"}
        /\ Send([type |-> "ack1pc", rm |-> r, outcome |-> rmState[r]])
        /\ UNCHANGED rmState
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet>>

(* A prepared participant asks the coordinator: resolve(txid). The answer
   is derived from the durable log only (presumed abort). *)
RMQuery(r) ==
  /\ rmState[r] = "prepared"
  /\ tcState \in {"committed", "aborted", "done"}
  /\ IF "commit" \in dlog /\ r \in commitSet
       THEN rmState' = [rmState EXCEPT ![r] = "committed"]
       ELSE /\ "commit" \notin dlog
            /\ rmState' = [rmState EXCEPT ![r] = "aborted"]
  /\ UNCHANGED <<tcState, dlog, vlog, commitSet, msgs>>

-----------------------------------------------------------------------------
Next ==
  \/ TCBegin \/ TCCommit \/ TCTrivialDone \/ TCAbort \/ TCDone \/ TCAbortDone
  \/ TCCrash \/ TCRecover
  \/ \E r \in RM :
       \/ TCOnePhase(r) \/ TCAdoptCommit(r)
       \/ \E o \in {"committed", "aborted"} : TC1PCAck(r, o)
       \/ RMPrepare(r) \/ RMReadOnly(r) \/ RMVoteNo(r) \/ RMCrash(r)
       \/ RMRcvCommit(r) \/ RMRcvAbort(r) \/ RMRcv1PC(r) \/ RMQuery(r)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(* Invariants *)

Consistent ==
  \A r, s \in RM : ~(rmState[r] = "committed" /\ rmState[s] = "aborted")

CommitPoint ==
  \A r \in RM :
    rmState[r] = "committed" /\ [type |-> "commit1pc", rm |-> r] \notin msgs
      => "commit" \in dlog

(* I2 as an action property: once the Commit record is durable, no NEW
   abort message is ever sent. (Messages are never removed, so this is
   stated over steps rather than states.) *)
NoAbortAfterCommit ==
  [][ "commit" \in dlog =>
        \A r \in RM :
          [type |-> "abort", rm |-> r] \in msgs' => [type |-> "abort", rm |-> r] \in msgs ]_vars

DoneIsFinal ==
  tcState = "done" =>
    \/ \A r \in RM : rmState[r] \in {"committed", "readonly"}
    \/ \A r \in RM : rmState[r] \in {"aborted", "readonly"}

=============================================================================
