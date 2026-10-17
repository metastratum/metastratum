-- People hold roles only at their own tenant or below it. Their account is
-- managed there (password, second factors, deletion, identity provider), so
-- a role higher up or beside it would let that tenant's administrators take
-- over a role they do not hold themselves.

-- Roles held elsewhere until now are removed.
DELETE FROM role_bindings b
USING users u, tenants home, tenants t
WHERE u.id = b.user_id AND home.id = u.tenant_id AND t.id = b.tenant_id
  AND NOT (home.path @> t.path);

CREATE FUNCTION mst_binding_in_home_subtree() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM users u
        JOIN tenants home ON home.id = u.tenant_id
        JOIN tenants t ON t.id = NEW.tenant_id
        WHERE u.id = NEW.user_id AND home.path @> t.path
    ) THEN
        RAISE EXCEPTION 'a role must be at the user''s own tenant or below it'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER role_bindings_in_home_subtree BEFORE INSERT OR UPDATE ON role_bindings
    FOR EACH ROW EXECUTE FUNCTION mst_binding_in_home_subtree();
