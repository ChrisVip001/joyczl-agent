//! One synthetic extraction sample; structural checks do not score emotional support.
use super::*;

const INPUT: &str =
    "我的称呼是小林，请只把这个称呼作为长期事实。今天有些疲惫，这是临时状态，不要存成长期事实。";

struct Capture {
    inner: Arc<dyn Provider>,
    output: Mutex<Vec<String>>,
}
impl Provider for Capture {
    fn create(
        &self,
        request: CreateRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<CreateResponse, ProviderError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let result = self.inner.create(request).await;
            if let Ok(response) = &result {
                self.output.lock().unwrap().push(response.text());
            }
            result
        })
    }
}

pub(super) async fn run(home: &Path, resolved: &Resolved) -> Result<Value> {
    let pool = joyczl_state::open(&home.join("state.db")).await?;
    let chat = joyczl_state::Chat::new(pool.clone());
    let facts = joyczl_state::Facts::new(pool.clone());
    let episodes = joyczl_state::Episodes::new(pool);
    chat.append_exchange(
        INPUT,
        "好的，我会区分长期称呼与临时状态。",
        "synthetic",
        "live-eval",
        None,
    )
    .await?;
    let capture = Capture {
        inner: resolved.client.clone(),
        output: Default::default(),
    };
    let result = joyczl_memory::consolidation::consolidate_if_due(
        &chat,
        &facts,
        &episodes,
        &capture,
        &resolved.small_model,
        1,
    )
    .await;
    let error = result.as_ref().err().map(ToString::to_string);
    let mut rows = Vec::new();
    let mut durable_count = 0;
    let mut nickname_count = 0;
    let mut temporary_quotes = 0;
    for fact in facts.recent(20, 0).await? {
        let durable = facts.is_durable(fact.id).await?;
        let (from, until) = facts.validity(fact.id).await?;
        let observations = facts.observations_detailed(fact.id).await?;
        let assessments: Vec<Value> = observations
            .iter()
            .filter_map(|row| row.3.as_deref())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        if durable {
            durable_count += 1;
            nickname_count +=
                usize::from(fact.subject.contains("小林") || fact.content.contains("小林"));
        } else if until.is_some()
            && assessments.iter().any(|item| {
                item["quote"]
                    .as_str()
                    .is_some_and(|quote| quote.contains("疲惫"))
            })
        {
            temporary_quotes += 1;
        }
        rows.push(json!({"id":fact.id,"subject":fact.subject,"content":fact.content,"source":fact.source,"kind":fact.kind,"durable":durable,"validFrom":from,"validUntil":until,"observations":observations,"assessments":assessments}));
    }
    let passed =
        error.is_none() && durable_count == 1 && nickname_count == 1 && temporary_quotes >= 1;
    let model_text = capture.output.lock().unwrap().clone();
    Ok(
        json!({"passed":passed,"input":INPUT,"newFacts":result.ok(),"error":error,"facts":rows,"durableFacts":durable_count,"nicknameFacts":nickname_count,"quotedTemporaryStates":temporary_quotes,"modelText":model_text,"scope":"One synthetic Chinese extraction: literal nickname retention, no extra durable fact, and a source-quoted expiring observation. Not an emotional-support, semantic-correctness or confidence-calibration score."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn the_metric_rejects_a_permanent_mood_even_when_a_temporary_copy_exists() -> Result<()> {
        for extra in [false, true] {
            let digest = joyczl_state::ledger::digest_of(
                serde_json::to_string(&("user", INPUT)).unwrap().as_bytes(),
            );
            let mut facts = vec![json!({"subject":"称呼","content":"称呼是小林","kind":"user"})];
            if extra {
                facts.push(json!({"subject":"心情","content":"用户疲惫","kind":"user"}));
            }
            let output = json!({"facts":facts,"states":[{"subject":"用户","content":"今天有些疲惫","observation":{"sourceRef":format!("chat:1@{digest}"),"quote":"今天有些疲惫","confidence":0.8,"ttlHours":2}}],"episode":"用户介绍称呼和暂时状态。"});
            let resolved = Resolved {
                client: Arc::new(joyczl_provider::mock::Mock::new(vec![
                    joyczl_provider::mock::Mock::text(&output.to_string()),
                ])),
                provider_id: "test".into(),
                model: "test".into(),
                small_model: "test".into(),
                vision: false,
                context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
            };
            let home = tempfile::tempdir()?;
            let report = run(home.path(), &resolved).await?;
            assert_eq!(report["passed"], !extra);
            assert_eq!(report["quotedTemporaryStates"], 1);
            assert_eq!(report["modelText"].as_array().unwrap().len(), 1);
        }
        Ok(())
    }
}
