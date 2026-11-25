use std::{
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use rig::tool::Tool;
use rig::tool::{ToolContext, ToolExecutionError};
use rig_agent::AgentBuilder;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::sources::{Reference, SearchArgs, Source, Sources};

#[derive(Deserialize, JsonSchema)]
pub(super) struct OpenAlexArgs {
    query: String,
    year_from: Option<u32>,
    year_to: Option<u32>,
    #[schemars(range(min = 1, max = 12))]
    limit: usize,
}

#[derive(Deserialize, JsonSchema)]
pub(super) struct QueryArgs {
    query: String,
    #[schemars(range(min = 1, max = 12))]
    limit: usize,
}

#[derive(Deserialize, JsonSchema)]
pub(super) struct WebArgs {
    query: String,
}

#[derive(Deserialize, JsonSchema)]
pub(super) struct DoiArgs {
    doi: String,
}

fn query(query: String, limit: usize) -> SearchArgs {
    SearchArgs {
        query,
        limit,
        year_from: None,
        year_to: None,
    }
}

impl From<OpenAlexArgs> for SearchArgs {
    fn from(args: OpenAlexArgs) -> Self {
        Self {
            year_from: args.year_from,
            year_to: args.year_to,
            ..query(args.query, args.limit)
        }
    }
}
impl From<QueryArgs> for SearchArgs {
    fn from(args: QueryArgs) -> Self {
        query(args.query, args.limit)
    }
}
impl From<WebArgs> for SearchArgs {
    fn from(args: WebArgs) -> Self {
        query(args.query, 6)
    }
}
impl From<DoiArgs> for SearchArgs {
    fn from(args: DoiArgs) -> Self {
        query(args.doi, 1)
    }
}

pub(super) trait ToolSpec: Send + Sync + 'static {
    type Args: JsonSchema + DeserializeOwned + Into<SearchArgs> + Send + Sync;
    const SOURCE: Source;
    const DESCRIPTION: &'static str;
}

macro_rules! tool_specs {
    ($($marker:ident($args:ty): $description:literal),+ $(,)?) => {$(
        pub(super) struct $marker;
        impl ToolSpec for $marker {
            type Args = $args;
            const SOURCE: Source = Source::$marker;
            const DESCRIPTION: &'static str = $description;
        }
    )+};
}

tool_specs! {
    OpenAlex(OpenAlexArgs): "Recherche dans OpenAlex par sujet, avec période, résumés et citations.",
    Crossref(QueryArgs): "Recherche bibliographique Crossref par sujet ou titre.",
    Hal(QueryArgs): "Recherche dans HAL, notamment pour la littérature francophone.",
    Arxiv(QueryArgs): "Recherche de prépublications arXiv par mots-clés.",
    Web(WebArgs): "Recherche web de littérature grise : rapports, normes, thèses.",
    ResolveDoi(DoiArgs): "Résout un DOI précis dans Crossref et retourne sa notice pour vérification.",
}
pub(super) use ResolveDoi as Doi;

pub(super) struct ToolState {
    sources: Arc<Sources>,
    agent: &'static str,
    remaining: AtomicUsize,
    year_from: Option<u32>,
    year_to: Option<u32>,
    evidence: Mutex<Vec<(String, Reference)>>,
}

impl ToolState {
    pub(super) fn new(
        sources: Arc<Sources>,
        agent: &'static str,
        budget: usize,
        year_from: Option<u32>,
        year_to: Option<u32>,
    ) -> Self {
        Self {
            sources,
            agent,
            remaining: AtomicUsize::new(budget),
            year_from,
            year_to,
            evidence: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn evidence(&self) -> Vec<(String, Reference)> {
        self.evidence
            .lock()
            .expect("registre bibliographique empoisonné")
            .clone()
    }

    async fn run(
        &self,
        source: Source,
        mut args: SearchArgs,
    ) -> Result<Observation, ToolExecutionError> {
        if self
            .remaining
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_err()
        {
            return Err(ToolExecutionError::refused(
                "Budget d’appels épuisé ; conclus avec les observations disponibles.",
            ));
        }
        args.limit = args.limit.clamp(1, 12);
        args.year_from = args.year_from.max(self.year_from);
        args.year_to = args.year_to.into_iter().chain(self.year_to).min();
        match self.sources.call(self.agent, source, args).await {
            Ok(refs) => {
                let mut evidence = self
                    .evidence
                    .lock()
                    .expect("registre bibliographique empoisonné");
                let mut references = Vec::with_capacity(refs.len());
                for reference in refs {
                    let id = format!("s{}", evidence.len() + 1);
                    references.push(json!({"id": &id, "notice": &reference}));
                    evidence.push((id, reference));
                }
                Ok(Observation { references })
            }
            Err(error) => {
                eprintln!("[{}] {} : {error:#}", self.agent, source.name());
                Err(ToolExecutionError::provider(format!(
                    "Source indisponible : {}",
                    source.name()
                )))
            }
        }
    }
}

#[derive(Serialize)]
pub(super) struct Observation {
    references: Vec<Value>,
}

pub(super) struct SearchTool<S> {
    state: Arc<ToolState>,
    spec: PhantomData<S>,
}

impl<S: ToolSpec> SearchTool<S> {
    pub(super) fn new(state: Arc<ToolState>) -> Self {
        Self {
            state,
            spec: PhantomData,
        }
    }
}

impl<S> SearchTool<S>
where
    S: ToolSpec,
    S::Args: From<QueryArgs>,
{
    pub(super) async fn search(
        &self,
        query: String,
        limit: usize,
    ) -> Result<Observation, ToolExecutionError> {
        self.call(
            &mut ToolContext::new(),
            S::Args::from(QueryArgs { query, limit }),
        )
        .await
    }
}

impl From<QueryArgs> for OpenAlexArgs {
    fn from(args: QueryArgs) -> Self {
        Self {
            query: args.query,
            limit: args.limit,
            year_from: None,
            year_to: None,
        }
    }
}

impl SearchTool<Doi> {
    pub(super) async fn resolve(&self, doi: String) -> Result<Observation, ToolExecutionError> {
        self.call(&mut ToolContext::new(), DoiArgs { doi }).await
    }
}

impl<S: ToolSpec> Tool for SearchTool<S> {
    const NAME: &'static str = S::SOURCE.name();
    type Args = S::Args;
    type Output = Observation;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        S::DESCRIPTION.to_owned()
    }

    fn parameters(&self) -> Value {
        schema_for!(S::Args).to_value()
    }

    async fn call(
        &self,
        _: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.state.run(S::SOURCE, args.into()).await
    }
}

pub(super) fn research_agent(builder: AgentBuilder, state: Arc<ToolState>) -> AgentBuilder {
    let builder = builder
        .tool(SearchTool::<OpenAlex>::new(Arc::clone(&state)))
        .tool(SearchTool::<Crossref>::new(Arc::clone(&state)))
        .tool(SearchTool::<Hal>::new(Arc::clone(&state)))
        .tool(SearchTool::<Arxiv>::new(Arc::clone(&state)));
    if state.sources.web_enabled() {
        builder.tool(SearchTool::<Web>::new(state))
    } else {
        builder
    }
}

pub(super) fn critic_agent(builder: AgentBuilder, state: Arc<ToolState>) -> AgentBuilder {
    builder
        .tool(SearchTool::<OpenAlex>::new(Arc::clone(&state)))
        .tool(SearchTool::<Doi>::new(state))
}
