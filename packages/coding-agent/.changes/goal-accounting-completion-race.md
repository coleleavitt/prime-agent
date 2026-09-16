Count the tokens of the turn that completes a goal.

Goal usage was attributed from the async agent-event queue, which raced the turn's own
ipython cell: when `goal.complete()` arrived over the kernel host bridge first, the goal was
no longer active by the time accounting ran, and the completing turn's tokens were dropped.
The completion event could report a finished goal with no tokens.

Tokens are now recorded synchronously when the assistant message ends, while the goal status
is exactly what it was for that turn. The budget limit is still decided later and once, and
only for a goal that is still active, so a turn that both completes a goal and crosses its
budget keeps its tokens without being flipped to budget_limited or sent a stale steer.
