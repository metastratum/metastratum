-- CPUs as feature lists, to tell which hosts run a VM's CPU model: each
-- host's (with the CPU description it was worked out from), and each VM's
-- model's.
ALTER TABLE hosts
    ADD COLUMN cpu_vendor text,
    ADD COLUMN cpu_features text[],
    ADD COLUMN cpu_features_of text;
ALTER TABLE vms ADD COLUMN cpu_features text[];
