# Rate limits

The API limits requests per client address (50 per second, bursts of 300),
password logins per address (one per 2 seconds, bursts of 30), and failed
logins per account (5, then one a minute; wrong second factors and wrong
passwords when changing a second factor count too). An account under attack
therefore cannot be guessed at quickly, but its user is slowed down, not
locked out: the budget refills. Refusals are HTTP 429 with `Retry-After`,
counted in `mst_rate_limited_total`.

Give the controllers a Redis (`api.redis_url`, `rediss://` for TLS) and they
share these budgets, whichever replica a client reaches, and keep them
across restarts. Redis holds nothing else: losing it loses nothing but the
budgets, and while it cannot be reached each replica limits on its own
(`mst_rate_limit_store_errors_total` counts the misses). Without Redis,
limits are per replica, so behind a load balancer spreading clients over N
replicas a client gets up to N times the budget. Account names are hashed
before they are used as keys.
