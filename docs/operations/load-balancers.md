# Load balancers

A load balancer is an address whose ports spread connections over member
VMs (Networks → Load balancers). OVN does it in the VPC's router and
switches: there are no appliance VMs, and nothing extra to run.
It works at the transport layer: TCP and UDP listeners, each forwarding a
port to a port on the members; TLS and HTTP routing are up to the members.

- **External**: the address comes from the VPC's external network, and is
  reachable from outside like a floating IP. The VPC keeps its external
  network while it has external load balancers.
- **Internal**: the address is in one of the VPC's networks, for its VMs.

Connections keep the client's address, so the members' security groups
decide who gets through (guardrails too). With health checks (the default)
OVN connects to each member's port every few seconds and leaves out those
that do not answer; the checks come from an address reserved in each
member's network (its highest free one), and OVN lets them past security
groups and guardrails (by its own MAC address, so nothing else from that
address gets through). Load balancers count against the tenant's
`load_balancers` quota.
