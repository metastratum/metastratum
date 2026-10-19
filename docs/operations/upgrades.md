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

0.5 does not upgrade earlier installations: install it afresh.

The release notes say when a release drops support for an agent revision.
