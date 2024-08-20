
This branch contains new integration tests specific to the replay_stage. It's currently a basic scenario, with intent to turn into a viable bench.

From core, you can run the replay_stage_test with:
```shell
RUST_LOG=trace cargo test --test replay_stage_test run_replay_stage_in_parallel
```

```shell
RUST_LOG=trace cargo test --test replay_stage_test run_replay_stage_in_serial
```

This branch also includes a design to replace the use of &RwLock<BankForks> and RwLock<PohRecorder> with channels.

See [bank_manager](core/src/bank_manager.rs) and [generate_new_bank_forks_with_channels in replay_stage](core/src/replay_stage.rs) for example usage. Please note,
this example is not fully wired up, so it's non-functional. It's just for looks.

This is just an idea to play around with to see if there are any performance improvements to be had. The basic idea is:

- No need for locks: The BankForks data is owned by a single thread (the BankManager), eliminating the need for RwLock.
- Simplified concurrency: Clients interact with BankForks through message passing, which can be easier to reason about than shared mutable state.
- Flexibility: You can easily add new operations by defining new message types.
- Potential for better performance: In some scenarios, message passing can be more efficient than lock contention, especially under high concurrency.
- Easy to distribute: This pattern can be extended to work across network boundaries if needed.

However, there are trade-offs:

- Increased latency: Each operation now involves sending a message and waiting for a response.
- More complex for simple operations: For read-heavy workloads with few writes, a RwLock might be simpler and more efficient.
- Memory usage: Keeping a channel and message queue might use more memory than a simple shared reference.