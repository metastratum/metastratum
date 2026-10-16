-- M5: certificate renewal. After a host renews its certificate the previous
-- one stays accepted, so an agent that has not yet stored the new one (it
-- crashed in between) can still connect and renew again.
ALTER TABLE hosts ADD COLUMN previous_cert_fingerprint text;
