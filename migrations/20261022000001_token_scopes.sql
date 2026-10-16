-- What an API token may do, as roles at tenants ({"tenant_id", "role"}),
-- on top of what its user may do. NULL: everything its user may.
ALTER TABLE api_tokens ADD COLUMN scopes jsonb;
