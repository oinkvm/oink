#include "unlock-policy.hpp"
#include <cassert>
int main() {
  UnlockOperation op;
  assert(!op.begin(99, 7, 0, false, 11)); // no startup/sign-in auto-logon
  assert(op.begin(1, 7, 100, true, 11));
  assert(!op.ready(2, 7, 110, 11, 109));
  assert(op.ready(1, 7, 110, 11, 109));
  assert(op.pending(7, 29999));
  assert(!op.pending(8, 200));
  assert(op.consume(1, 7, 200));
  assert(!op.consume(1, 7, 201));
  op.result(1, 7, false);
  // A failed attempt leaves a notice but the next accepted entry may retry.
  assert(op.begin(2, 7, 203, true, 11));
  op.reset();
  assert(op.begin(2, 7, 500, true, 11));
  assert(op.ready(2, 7, 501, 11, 499));
  assert(!op.pending(7, 30500));
  assert(!op.consume(2, 7, 30500));
  op.reset();
  assert(op.begin(3, 7, 100, true, 11));
  assert(op.ready(3, 7, 101, 11, 100));
  assert(op.consume(3, 7, 102));
  op.result(3, 8, true);
  assert(!op.own_unlock(7, 103));
  op.result(3, 7, true);
  assert(op.own_unlock(7, 103));
  assert(!op.own_unlock(7, 104));
  // Save/Disable invalidate prior provider instances and preparation workers.
  op.reset();
  assert(op.begin(10, 7, 100, true, 11));
  op.reset(); // Save: old pending must not consume the replacement secret.
  assert(!op.ready(10, 7, 101, 11, 100));
  assert(!op.consume(10, 7, 102));
  assert(op.begin(11, 7, 103, true, 11));
  assert(!op.ready(10, 7, 104, 11, 103));
  assert(!op.pending(7, 104));
  assert(op.ready(11, 7, 105, 11, 104));
  assert(!op.consume(10, 7, 106));
  assert(op.consume(11, 7, 106));
  op.result(10, 7, true);
  assert(op.state == UnlockOperation::Submitted);
  op.reset(); // Disable: stale CP cannot acquire after acknowledged deletion.
  assert(!op.consume(11, 7, 107));
  // Crash/restart never restores a submission or loses a failure indication.
  assert(UnlockOperation::recover_saved(UnlockOperation::Pending) ==
         UnlockOperation::Expired);
  assert(UnlockOperation::recover_saved(UnlockOperation::Submitted) ==
         UnlockOperation::Failed);
  assert(UnlockOperation::recover_saved(UnlockOperation::Failed) ==
         UnlockOperation::Failed);
  assert(UnlockOperation::recover_saved(UnlockOperation::Expired) ==
         UnlockOperation::Expired);
  assert(UnlockOperation::recover_saved(UnlockOperation::Succeeded) ==
         UnlockOperation::Succeeded);

  // Provider readiness is a one-shot decision. No remote input, stale input,
  // input from another source, and a clock that appears to run backwards all
  // fail closed and cannot become eligible on a later poll.
  op.reset();
  assert(op.begin(20, 7, 1000, true, 21));
  assert(op.ready(20, 7, 1001, 0, 0));
  assert(!op.pending(7, 1001));
  assert(op.ready(20, 7, 1002, 21, 1002));
  assert(!op.pending(7, 1002));

  op.reset();
  assert(op.begin(21, 7, 1000, true, 21));
  assert(op.ready(21, 7, 3001, 21, 1000));
  assert(!op.pending(7, 3001));

  op.reset();
  assert(op.begin(22, 7, 1000, true, 21));
  assert(op.ready(22, 7, 1001, 22, 1000));
  assert(!op.pending(7, 1001));

  op.reset();
  assert(op.begin(23, 7, 1000, true, 21));
  assert(!op.ready(23, 7, 999, 21, 999));
  assert(!op.pending(7, 999));
}
