//! Bounded audit reads; sequence watermarks exclude concurrently appended events.
use super::*;

impl Store {
    pub async fn event_page(
        &self,
        id: &str,
        after: i64,
        through: Option<i64>,
        limit: i64,
    ) -> Result<Value> {
        ensure!(
            after >= 0 && (1..=20).contains(&limit),
            "invalid event cursor or page limit"
        );
        self.get_loop(id).await?;
        let high: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(sequence),0) FROM events WHERE loop_id=?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        let through = through.unwrap_or(high);
        ensure!(
            through >= after && through <= high,
            "invalid event upper bound"
        );
        let rows = sqlx::query("SELECT sequence,kind,CASE WHEN length(CAST(body AS BLOB))<=8192 THEN body ELSE NULL END AS body,length(CAST(body AS BLOB)) AS byte_length,created_at FROM events WHERE loop_id=? AND sequence>? AND sequence<=? ORDER BY sequence LIMIT ?")
            .bind(id).bind(after).bind(through).bind(limit+1).fetch_all(&self.pool).await?;
        let more = rows.len() > limit as usize;
        let items: Vec<Value> = rows.into_iter().take(limit as usize).map(|r| {
            let body: Option<String> = r.get("body");
            Ok(json!({"sequence":r.get::<i64,_>("sequence"),"kind":r.get::<String,_>("kind"),"body":body.as_deref().map(serde_json::from_str::<Value>).transpose()?,"body_omitted":body.is_none(),"body_byte_length":r.get::<i64,_>("byte_length"),"created_at":r.get::<i64,_>("created_at")}))
        }).collect::<Result<_>>()?;
        let next = if more {
            items.last().map(|v| v["sequence"].clone())
        } else {
            None
        };
        Ok(json!({"items":items,"next_after_sequence":next,"through_sequence":through}))
    }

    pub async fn event_body(
        &self,
        id: &str,
        sequence: i64,
        offset: usize,
        limit: usize,
    ) -> Result<Value> {
        ensure!(
            sequence > 0 && (1..=32768).contains(&limit),
            "invalid event body page"
        );
        let offset = i64::try_from(offset)?;
        let row = sqlx::query("SELECT length(CAST(body AS BLOB)) AS byte_length,substr(CAST(body AS BLOB),?,?) AS bytes FROM events WHERE loop_id=? AND sequence=?")
            .bind(offset.checked_add(1).context("offset overflow")?).bind(limit as i64).bind(id).bind(sequence).fetch_optional(&self.pool).await?.context("event not found")?;
        let length: i64 = row.get("byte_length");
        ensure!(offset <= length, "offset beyond event body");
        let bytes: Vec<u8> = row.get("bytes");
        let end = offset + bytes.len() as i64;
        Ok(
            json!({"sequence":sequence,"offset":offset,"byte_length":length,"text":std::str::from_utf8(&bytes).ok(),"bytes":bytes,"next_offset":if end < length {Some(end)} else {None}}),
        )
    }
}
