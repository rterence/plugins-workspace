---
"shell": patch
---

Replace the polling loop in the child wait thread with a blocking wait on the process itself, removing a 10ms poll per spawned child and the corresponding latency on `Terminated` events. `CommandChild::kill` no longer makes blocking calls while holding the child lock, so it can never stall the wait thread (or, on Windows, park in the job-object wait with the lock held).
