#pragma once
#include <cstdint>
// Service-owned ledger. Enumeration is read-only; consume precedes decryption.
// Unknown outcomes remain blocked until a new local Save. No operation survives
// service restart, and a durable failure latch is set before handing out a
// secret.
struct UnlockOperation {
  static constexpr uint64_t RemoteInputWindowMs = 2000;
  // Pointer operations survive LogonUI re-enumeration for 30 s. A sync unlock
  // operation is abandoned if LogonUI has not taken its credential 3 s after
  // the trigger (user decision Q3: strict, no 30 s preparation).
  static constexpr uint64_t PendingTtlMs = 30000, SyncPendingTtlMs = 3000;
  enum State : uint32_t {
    Idle,
    Pending,
    Submitted,
    Succeeded,
    Failed,
    Expired
  };
  uint64_t id = 0, start = 0;
  uint32_t session = 0;
  State state = Idle;
  bool prepared = false, eligible = false, observed = false, sync = false;
  uint64_t source = 0, ready_at = 0, remote_input_at = 0,
           remote_input_source = 0, succeeded_at = 0;
  static State recover_saved(State prior) {
    if (prior == Pending)
      return Expired;
    if (prior == Submitted)
      return Failed;
    return prior;
  }
  void reset() { *this = UnlockOperation{}; }
  uint64_t ttl() const {
    // Once LogonUI consumed a sync credential, its outcome gets the ordinary
    // window: only the wait for serialization is short.
    return sync && state == Pending ? SyncPendingTtlMs : PendingTtlMs;
  }
  bool fresh(uint32_t s, uint64_t now) const {
    return s == session && now >= start && now - start < ttl();
  }
  bool begin(uint64_t i, uint32_t s, uint64_t now, bool signed_in,
             uint64_t source_tag, bool sync_operation = false) {
    // A completed failure is a durable UI notice, not a feature disable. The
    // next accepted pointer entry may start a fresh operation.
    if (!signed_in || !i || !s || s == UINT32_MAX || state == Submitted ||
        state == Pending || source_tag == 0)
      return false;
    id = i;
    session = s;
    start = now;
    source = source_tag;
    state = Pending;
    sync = sync_operation;
    succeeded_at = 0;
    prepared = false;
    eligible = false;
    ready_at = 0;
    remote_input_at = 0;
    remote_input_source = 0;
    observed = false;
    return true;
  }
  bool ready(uint64_t i, uint32_t s, uint64_t now, uint64_t input_source,
             uint64_t input_at) {
    if (i != id || state != Pending || !fresh(s, now))
      return false;
    if (!prepared) {
      prepared = true;
      ready_at = now;
      remote_input_at = input_at;
      remote_input_source = input_source;
      // A sync operation was authorized by the source's UnlockReply before
      // Trigger; that replaces the two-second remote input rule. LogonUI
      // calling GetSerialization within the sync window is the readiness test.
      eligible = sync || (input_source == source && input_at <= now &&
                          now - input_at <= RemoteInputWindowMs);
    }
    return true;
  }
  bool pending(uint32_t s, uint64_t now) const {
    return state == Pending && prepared && eligible && fresh(s, now);
  }
  bool consume(uint64_t i, uint32_t s, uint64_t now) {
    if (i != id || !pending(s, now))
      return false;
    state = Submitted;
    return true;
  }
  void result(uint64_t i, uint32_t s, bool success, uint64_t now) {
    if (i == id && s == session && state == Submitted) {
      state = success ? Succeeded : Failed;
      if (success)
        succeeded_at = now;
    }
  }
  // An unconsumed sync operation that expired is a skip, not a failure.
  bool sync_abandoned() const { return sync && state == Expired; }
  bool own_unlock(uint32_t s, uint64_t now) {
    if (state != Succeeded || observed || !fresh(s, now))
      return false;
    observed = true;
    return true;
  }
  void expire(uint64_t now) {
    if ((state == Pending || state == Submitted) && !fresh(session, now))
      state = state == Submitted ? Failed : Expired;
  }
};
