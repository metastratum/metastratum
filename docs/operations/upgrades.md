# Upgrades

Controllers and agents say which protocol revision they speak and which
commands an agent handles. Upgrade **controllers first, then agents**:

1. Upgrade the controllers one at a time. A new controller serves older
   agents (anything from the oldest revision it supports; it refuses older
   ones with a message naming both), and does not send them commands they
   do not handle; the action fails with "upgrade the agent" instead.
   Database migrations run when the first new controller starts. They are
   kept compatible with the previous release's controllers (adding to the
   schema, and only removing what that release no longer uses), and older
   replicas ignore migrations they do not know, so they keep working, and
   can restart, until they are replaced.
2. Upgrade the agents, a host at a time or all at once. Restarting an agent
   does not touch its VMs. Hosts can be drained to upgraded hosts
   throughout.

Controllers keep the fields of VM definitions that newer controllers
added when they write a definition back: replicas of two versions side by
side do not undo each other's changes.

0.5 does not upgrade earlier installations: install it afresh. 0.5 and
0.6 do not start on a new, empty database; install 0.7 or later afresh.

From 0.5 to 0.6:

- **Stop every 0.5 controller before starting the first 0.6 one.** Its
  migrations change the images table under 0.5 controllers (their image
  calls would fail, and they would fail 0.6's image tasks), so this is
  not a rolling upgrade; agents still are (they keep their VMs running
  meanwhile).
- Hosts no longer download images: the controller does, as an image is
  added, and never again (images keep no URL). Images being downloaded as the
  first 0.6 controller starts fail (delete them and add them again), and
  so do images no copy of which is left (every host holding one was
  decommissioned; 0.5 would have downloaded them again). The URLs they
  came from are in the audit log (`image.create`).
- Adding images needs upgraded agents (`receive_image`, `store_image`):
  until a host's agent is upgraded, images are not added there.
- Secure Boot and TPMs need `swtpm` (and `swtpm-tools`) on hosts; the
  0.6 installer and agent package bring them. Hosts without them say so
  (No TPM, No Secure Boot), and VMs that need them are not placed there.

From 0.6 to 0.7:

- A rolling upgrade: 0.6 controllers keep working beside 0.7 ones (its
  one migration gives images a sleep setting, which they ignore). A VM
  asleep on an upgraded host shows as unknown through a 0.6 controller.
- Until a host's agent is upgraded, VMs started there may sleep whatever
  their setting (see [Sleep](sleep.md)), and one asleep shows as Paused
  and is not woken by starting it (`virsh dompmwakeup` on the host does).

The release notes say when a release drops support for an agent revision.
