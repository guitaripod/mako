use worker::*;
use crate::error::AppError;

pub async fn check_and_acquire_lock(app_id: &str, user_id: &str, db: &D1Database) -> Result<()> {
    // Try to insert a lock record - will fail if one already exists
    let result = db
        .prepare("INSERT INTO user_locks (app_id, user_id, acquired_at) VALUES (?, ?, datetime('now'))")
        .bind(&[app_id.into(), user_id.into()])?
        .run()
        .await;

    match result {
        Ok(_) => Ok(()),
        Err(_) => {
            // Lock exists, check if it's stale (older than 60 seconds)
            let stale_check = db
                .prepare("DELETE FROM user_locks WHERE app_id = ? AND user_id = ? AND acquired_at < datetime('now', '-60 seconds')")
                .bind(&[app_id.into(), user_id.into()])?
                .run()
                .await?;

            if stale_check.meta().is_ok() && stale_check.meta().unwrap().is_some() && stale_check.meta().unwrap().unwrap().changes.unwrap_or(0) > 0 {
                // Stale lock removed, try again
                db.prepare("INSERT INTO user_locks (app_id, user_id, acquired_at) VALUES (?, ?, datetime('now'))")
                    .bind(&[app_id.into(), user_id.into()])?
                    .run()
                    .await
                    .map(|_| ())
                    .map_err(|_| worker::Error::from(AppError::RateLimitExceeded))
            } else {
                Err(worker::Error::from(AppError::RateLimitExceeded))
            }
        }
    }
}

pub async fn release_lock(app_id: &str, user_id: &str, db: &D1Database) -> Result<()> {
    db.prepare("DELETE FROM user_locks WHERE app_id = ? AND user_id = ?")
        .bind(&[app_id.into(), user_id.into()])?
        .run()
        .await?;
    Ok(())
}

pub async fn enforce_write_rate_limit(env: &Env, app_id: &str, user_id: &str, label: &str) -> std::result::Result<(), AppError> {
    enforce(env, "TENANT_WRITE_LIMITER", &format!("w:{}:{}", app_id, user_id), label).await
}

pub async fn enforce_read_rate_limit(env: &Env, app_id: &str, user_id: &str, label: &str) -> std::result::Result<(), AppError> {
    enforce(env, "READ_LIMITER", &format!("r:{}:{}", app_id, user_id), label).await
}

/// Per-(app,user) rate limit. Fails OPEN — a missing binding or a limiter error
/// never blocks a request. On exceed it logs the would-be-block and only errors
/// when RL_ENFORCE is "true"; otherwise it is shadow mode (observe, no block).
async fn enforce(env: &Env, binding: &str, key: &str, label: &str) -> std::result::Result<(), AppError> {
    let limiter = match env.get_binding::<RateLimiter>(binding) {
        Ok(l) => l,
        Err(_) => return Ok(()),
    };
    match limiter.limit(key.to_string()).await {
        Ok(outcome) if !outcome.success => {
            let enforce = env.var("RL_ENFORCE").map(|v| v.to_string() == "true").unwrap_or(false);
            console_log!("rate-limit exceeded: {} key={} enforce={}", label, key, enforce);
            if enforce {
                Err(AppError::RateLimitExceeded)
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// The client network a per-address rate limit counts against: an IPv4 address as is (also when written as an
/// IPv4-mapped IPv6 address), and an IPv6 address as its /64 prefix, since one subscriber usually holds a whole /64.
/// Mirrors `networkOf` in services/lib/src/rateLimit.ts. A value that is not an address is kept unchanged.
pub fn client_network(ip: &str) -> String {
    match ip.trim().parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Ok(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
        Err(_) => ip.to_string(),
    }
}

#[cfg(test)]
mod client_network_tests {
    use super::client_network;

    #[test]
    fn every_address_of_one_ipv6_64_shares_a_key() {
        assert_eq!(client_network("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2::/64");
        assert_eq!(client_network("2001:db8:1:2:ffff:ffff:ffff:ffff"), "2001:db8:1:2::/64");
        assert_eq!(client_network("2001:db8::1"), "2001:db8:0:0::/64");
        assert_ne!(client_network("2001:db8:1:3::1"), client_network("2001:db8:1:2::1"));
    }

    #[test]
    fn ipv4_keeps_its_own_key_also_when_mapped() {
        assert_eq!(client_network("203.0.113.5"), "203.0.113.5");
        assert_eq!(client_network("::ffff:203.0.113.5"), "203.0.113.5");
        assert_eq!(client_network("unknown"), "unknown");
    }
}
