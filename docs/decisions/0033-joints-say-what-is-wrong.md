# 0033: Joints say their torque and what's wrong with them, in their state and their node's log

**Status:** Proposed, 2026-10-25

## Context
drydock breaks motors on purpose (a dead drive, a weak one, a frozen
encoder), and keel-mcp gives an agent the dataflow's status, logs and
latency ([0030](0030-agent-tools.md)). Nothing in between noticed a broken
joint. With OpenArm's elbow dead, the joint dropped to its stop and every
node went on running, every link moving, no line in any log.

What tells a broken drive apart is its torque: a dead one gives none, a weak
one a fraction of what it's asked, and one closing its loop on a frozen
encoder pushes as hard as it may. Damiao's motors report their torque in
every answer, and their own errors in their status; `State` dropped both.

## Decision
- **`State` carries `effort`**: the torque the drive gives, as it measures
  it, in N m. Damiao's motors report it; the simulated pendulums give the
  torque they stepped with; made-up drives, whose frames don't carry it, the
  torque last sent.
- **And a `status`**: flags (drive error, disabled, underpowered, behind,
  frozen) and the drive's own code, as its maker numbers it (Damiao's
  status). 32 bytes a joint. Errors and warnings go upstream with the state:
  a controller, a recorder, a replay see them.
- **The joints' loop watches its joints** (`keel_control::health`, run by
  `JointLoop` on every state it reads): each against the command it was last
  given. When a joint has, for half a second, given far less torque than its
  command's law asks (under 30%, when at least 1 N m is asked), stayed far
  from its target (0.2 rad), or read the same while its target moved, its
  status says so, and the node's log says it in words, with the numbers, and
  again when it's over; a summary every 5 s. Nothing counts in the first
  2 s, while joints settle.
- **No central watcher.** Each component says what's wrong with what it
  knows: the drive in its status, the joints' node in the state and its
  log. An agent reads every component's log with `keel-mcp`'s `logs`; a
  person with `keel logs arm`. A separate watching node is for logic no one
  component has: the robot fell (an IMU), a camera froze, the arm never got
  where it was sent.
- **Facts, not verdicts.** "gives 0.0 N m of the -52.4 asked", not "the
  motor is dead": the reader concludes, as with `keel web`'s quiet links.
- **Scenarios** in `examples/agent/run.sh`: `openarm-dead`, `-weak`,
  `-freeze`, and `-healthy`, OpenArm against drydock with a motor broken 4 s
  in (or not). The question doesn't say anything is wrong: "Is anything wrong
  with it?" Graded on naming the joint and the kind of fault, or finding
  nothing.

## Consequences
- What the arm's node says, against drydock: a dead elbow, "gives 0.0 N m of
  the -52.4 asked; +0.87 rad from its target"; a weak one, "gives -4.3 N m of
  the -21.6 asked", a fifth; a frozen first joint, its reading still while
  its target swings. A healthy arm: only the summaries.
- A weak motor shows only under load: weakened, a joint that carries little
  still holds, and nothing is said. The scenario weakens the elbow.
- A gripper told to close starts moving a little after its target jumps:
  a still reading counts only once the target has moved away from it for
  half a second, or every grip would be a frozen sensor.
- The four scenarios passed, with Sonnet, twice: first with the watch as a
  node of its own, then in the joints' loop. The dead and weak elbow named
  with their torques; the frozen joint as stuck or frozen feedback (the agent
  named both, rightly unsure which); the healthy arm with nothing wrong but
  this laptop's occasional missed deadline, put down to its real-time
  settings. Graded by keywords, leniently: it shows the evidence is there,
  not how good an agent is.
- Agents notice what isn't flagged too: the gripper's swing was mentioned
  as odd, then dismissed because the log said it followed.
- Recordings of states from before this don't read as states any more: the
  layout grew from 16 bytes to 32.
- `keel-can` and `keel-sim` watch their joints with no setting; the made-up
  drives and the pendulums give what they're asked, so they never show
  underpowered.
- Not yet: acting on it (disable a joint, stop the arm, with a person
  approving, as `restart_node` does), thresholds per joint, a joint's limits
  (its peak torque) to tell a weak drive from a saturated one, and joints
  that stop answering (the loop already says when no whole state comes for
  a second).
- From the archived first simulation (its `arm-health`), not taken: a check
  that states keep coming at their rate, and a hold-off before a joint
  "follows again", which would quiet a frozen joint's lines as its target
  swings past.
