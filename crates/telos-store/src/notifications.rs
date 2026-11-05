use super::*;

#[derive(Debug, Clone)]
pub struct NotificationLease {
    pub id: i64,
    pub token: String,
    pub event_sequence: i64,
    pub loop_id: Option<String>,
    pub kind: String,
    pub body: Value,
}

impl Store {
    pub async fn notification_status(&self) -> Result<Vec<Value>> {
        let rows =
            sqlx::query("SELECT state,COUNT(*) AS total FROM outbox GROUP BY state ORDER BY state")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|r| json!({"state":r.get::<String,_>("state"),"count":r.get::<i64,_>("total")}))
            .collect())
    }

    pub async fn claim_notification(&self, at: i64) -> Result<Option<NotificationLease>> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT o.*,e.loop_id,e.kind,e.body FROM outbox o JOIN events e ON e.sequence=o.event_sequence WHERE (o.state='pending' AND o.next_attempt_at<=?) OR (o.state='sending' AND o.lease_until<=?) ORDER BY o.id LIMIT 1")
            .bind(at).bind(at).fetch_optional(&mut *tx).await?;
        let Some(row) = row else { return Ok(None) };
        let id: i64 = row.get("id");
        let loop_id: Option<String> = row.get("loop_id");
        if row.get::<i64, _>("attempts") >= 5 {
            sqlx::query("UPDATE outbox SET state='failed',lease_token=NULL,lease_until=NULL,last_error='delivery lease expired after final attempt' WHERE id=?").bind(id).execute(&mut *tx).await?;
            Self::event(
                &mut tx,
                loop_id.as_deref(),
                "notification_failed",
                json!({"notification_id":id,"reason":"retry budget exhausted"}),
                false,
            )
            .await?;
            tx.commit().await?;
            return Ok(None);
        }
        let token = Uuid::new_v4().to_string();
        sqlx::query("UPDATE outbox SET state='sending',attempts=attempts+1,lease_token=?,lease_until=? WHERE id=?")
            .bind(&token).bind(at.checked_add(30).context("notification lease overflow")?).bind(id).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            loop_id.as_deref(),
            "notification_claimed",
            json!({"notification_id":id,"attempt":row.get::<i64,_>("attempts")+1}),
            false,
        )
        .await?;
        let lease = NotificationLease {
            id,
            token,
            event_sequence: row.get("event_sequence"),
            loop_id,
            kind: row.get("kind"),
            body: serde_json::from_str(&row.get::<String, _>("body"))?,
        };
        tx.commit().await?;
        Ok(Some(lease))
    }

    pub async fn finish_notification(
        &self,
        lease: &NotificationLease,
        error: Option<&str>,
        at: i64,
    ) -> Result<()> {
        ensure!(
            error.is_none_or(|s| s.len() <= 4096),
            "notification error too large"
        );
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT attempts FROM outbox WHERE id=? AND state='sending' AND lease_token=? AND lease_until>?")
            .bind(lease.id).bind(&lease.token).bind(at).fetch_optional(&mut *tx).await?.context("notification lease is stale")?;
        let attempts: i64 = row.get("attempts");
        let state = if error.is_none() {
            "sent"
        } else if attempts >= 5 {
            "failed"
        } else {
            "pending"
        };
        let due = at
            .checked_add(1i64 << attempts.min(8))
            .context("notification retry overflow")?;
        sqlx::query("UPDATE outbox SET state=?,lease_token=NULL,lease_until=NULL,next_attempt_at=?,last_error=? WHERE id=?")
            .bind(state).bind(due).bind(error).bind(lease.id).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            lease.loop_id.as_deref(),
            "notification_delivery",
            json!({"notification_id":lease.id,"state":state,"attempt":attempts,"error":error}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
