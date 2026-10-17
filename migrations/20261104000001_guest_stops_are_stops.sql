-- VMs shut down or powered off other than through the API are meant to be
-- stopped from now on (see vms::observe_power); those stopped so before
-- this change are too.
UPDATE vms SET desired_power = 'stopped', updated_at = now()
 WHERE desired_power = 'running' AND power = 'stopped'
   AND power_reason IN ('shutdown', 'destroyed') AND status = 'ready';
