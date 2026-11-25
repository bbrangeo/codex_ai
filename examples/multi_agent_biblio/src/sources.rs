use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Request, RequestBuilder, StatusCode, Url, header::RETRY_AFTER};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RETRIES: u32 = 2;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("codex_ai-biblio/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub(super) struct Reference {
    pub(super) title: String,
    pub(super) authors: Vec<String>,
    pub(super) year: Option<u32>,
    pub(super) venue: Option<String>,
    pub(super) doi: Option<String>,
    pub(super) citations: Option<u64>,
    pub(super) open_access: Option<bool>,
    pub(super) abstract_text: Option<String>,
    pub(super) url: Option<String>,
    pub(super) source_tool: String,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Source {
    OpenAlex,
    Crossref,
    ResolveDoi,
    Hal,
    Arxiv,
    Web,
}

impl Source {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::OpenAlex => "search_openalex",
            Self::Crossref => "search_crossref",
            Self::ResolveDoi => "resolve_doi",
            Self::Hal => "search_hal",
            Self::Arxiv => "search_arxiv",
            Self::Web => "web_search",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Arxiv => ".xml",
            _ => ".json",
        }
    }
}

#[derive(Debug)]
pub(super) struct SearchArgs {
    pub(super) query: String,
    pub(super) year_from: Option<u32>,
    pub(super) year_to: Option<u32>,
    pub(super) limit: usize,
}

pub(super) struct Sources {
    client: Client,
    openalex_tls12: Client,
    openalex_mailto: Option<String>,
    crossref_agent: String,
    tavily_key: Option<String>,
    cache: Option<PathBuf>,
    dump: Option<PathBuf>,
}

impl Sources {
    pub(super) fn new(cache: Option<PathBuf>, dump: Option<PathBuf>) -> Result<Self> {
        for dir in cache.iter().chain(&dump) {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let client = || {
            Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
        };
        let openalex_mailto = env_value("OPENALEX_MAILTO");
        let crossref_agent = env_value("CROSSREF_MAILTO")
            .or_else(|| openalex_mailto.clone())
            .map_or_else(
                || USER_AGENT.to_owned(),
                |email| format!("{USER_AGENT} (mailto:{email})"),
            );
        Ok(Self {
            client: client().build()?,
            openalex_tls12: client()
                .max_tls_version(reqwest::tls::Version::TLS_1_2)
                .build()?,
            openalex_mailto,
            crossref_agent,
            tavily_key: env_value("TAVILY_API_KEY"),
            cache,
            dump,
        })
    }

    pub(super) fn web_enabled(&self) -> bool {
        self.tavily_key.is_some()
    }

    pub(super) async fn call(
        &self,
        agent: &str,
        source: Source,
        args: SearchArgs,
    ) -> Result<Vec<Reference>> {
        let start = Instant::now();
        let result = self.fetch(source, &args).await;
        eprintln!(
            "[{agent}] tool={} q={:?} → {} résultats ({} ms)",
            source.name(),
            args.query,
            result.as_ref().map_or(0, Vec::len),
            start.elapsed().as_millis(),
        );
        result.with_context(|| source.name())
    }

    async fn fetch(&self, source: Source, args: &SearchArgs) -> Result<Vec<Reference>> {
        if args.limit == 0 && !matches!(source, Source::ResolveDoi) {
            return Ok(Vec::new());
        }
        let request = self
            .request(source, args)?
            .build()
            .map_err(reqwest::Error::without_url)?;
        let prefix = format!("{}-", source.name());
        let cached = self.cache.as_ref().map(|dir| {
            let key = request_key(&request);
            dir.join(format!("{prefix}{key}{}", source.extension()))
        });
        if let Some(path) = &cached
            && let Some(body) = read_cache(path.clone()).await?
            && let Ok(references) = parse(source, &body, args)
        {
            self.dump(source, format!("{prefix}cache-"), &body).await?;
            return Ok(references);
        }

        let mut attempt = 0;
        loop {
            let replay = || {
                request
                    .try_clone()
                    .context("request body cannot be replayed")
            };
            let response = match self.client.execute(replay()?).await {
                // Some OpenAlex edges reset the default TLS handshake. Retry this
                // idempotent GET once over TLS 1.2, still verifying certificates.
                Err(error) if matches!(source, Source::OpenAlex) && error.is_connect() => {
                    self.openalex_tls12.execute(replay()?).await
                }
                result => result,
            }
            .map_err(reqwest::Error::without_url)?;
            let status = response.status();
            let retry = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| retry_after(value.to_str().ok()?, SystemTime::now()));
            let body = response
                .bytes()
                .await
                .map_err(reqwest::Error::without_url)?;
            self.dump(source, format!("{prefix}{}-", status.as_u16()), &body)
                .await?;
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < RETRIES {
                let delay = retry.unwrap_or_else(|| Duration::from_millis(250 << attempt));
                // Do not retry earlier than requested, even if the server asks for a long wait.
                if delay <= MAX_RETRY_DELAY {
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
            }
            if matches!(source, Source::ResolveDoi) && status == StatusCode::NOT_FOUND {
                return Ok(Vec::new());
            }
            ensure!(status.is_success(), "HTTP {status}");
            let references = parse(source, &body, args)?;
            if let Some(path) = cached {
                write_raw(path, None, source.extension(), body.to_vec()).await?;
            }
            return Ok(references);
        }
    }

    async fn dump(&self, source: Source, prefix: String, body: &[u8]) -> Result<()> {
        if let Some(dir) = &self.dump {
            write_raw(dir.clone(), Some(prefix), source.extension(), body.to_vec()).await?;
        }
        Ok(())
    }

    fn request(&self, source: Source, args: &SearchArgs) -> Result<RequestBuilder> {
        let query = args.query.trim();
        ensure!(!query.is_empty(), "empty query");
        let limit = args.limit;
        if !matches!(source, Source::ResolveDoi) {
            ensure!(
                args.year_from.is_none_or(|year| (1..=9999).contains(&year))
                    && args.year_to.is_none_or(|year| (1..=9999).contains(&year))
                    && args.year_from.unwrap_or(1) <= args.year_to.unwrap_or(9999),
                "invalid year range",
            );
        }
        let get = |url| self.client.get(url);
        let request = match source {
            Source::OpenAlex => {
                let mut req = get("https://api.openalex.org/works")
                    .query(&[("search", query)])
                    .query(&[("per-page", limit.min(200))])
                    .query(&[("select", "id,doi,title,authorships,publication_year,primary_location,cited_by_count,open_access,abstract_inverted_index")]);
                if let Some(email) = &self.openalex_mailto {
                    req = req.query(&[("mailto", email)]);
                }
                date_filter(req, args, "from_publication_date", "to_publication_date")
            }
            Source::Crossref => date_filter(
                get("https://api.crossref.org/works")
                    .header(reqwest::header::USER_AGENT, &self.crossref_agent)
                    .query(&[("query.bibliographic", query)])
                    .query(&[("rows", limit.min(1000))]),
                args,
                "from-pub-date",
                "until-pub-date",
            ),
            Source::ResolveDoi => {
                let doi = normalize_doi(query).context("invalid DOI")?;
                let mut url = Url::parse("https://api.crossref.org/works/")?;
                url.path_segments_mut()
                    .map_err(|()| anyhow::anyhow!("invalid Crossref base URL"))?
                    .pop_if_empty()
                    .push(&doi);
                self.client
                    .get(url)
                    .header(reqwest::header::USER_AGENT, &self.crossref_agent)
            }
            Source::Hal => {
                let mut req = get("https://api.archives-ouvertes.fr/search/")
                    .query(&[("q", solr_literal(query)), ("wt", "json".to_owned())])
                    .query(&[("fl", "title_s,authFullName_s,producedDateY_i,journalTitle_s,conferenceTitle_s,bookTitle_s,doiId_s,openAccess_bool,abstract_s,uri_s")])
                    .query(&[("rows", limit.min(1000))]);
                if args.year_from.is_some() || args.year_to.is_some() {
                    let bound = |year: Option<u32>| year.map_or("*".to_owned(), |y| y.to_string());
                    let filter = format!(
                        "producedDateY_i:[{} TO {}]",
                        bound(args.year_from),
                        bound(args.year_to)
                    );
                    req = req.query(&[("fq", filter)]);
                }
                req
            }
            Source::Arxiv => {
                let mut search = format!(
                    r#"all:"{}""#,
                    query.replace('\\', "\\\\").replace('"', "\\\"")
                );
                if args.year_from.is_some() || args.year_to.is_some() {
                    let from = args.year_from.unwrap_or(1);
                    let to = args.year_to.unwrap_or(9999);
                    search.push_str(&format!(
                        " AND submittedDate:[{from:04}01010000 TO {to:04}12312359]"
                    ));
                }
                get("https://export.arxiv.org/api/query")
                    .query(&[("search_query", search)])
                    .query(&[("max_results", limit.min(2000))])
            }
            Source::Web => {
                let key = self
                    .tavily_key
                    .as_deref()
                    .context("TAVILY_API_KEY is not configured")?;
                let mut body = json!({"query": query, "max_results": limit.min(20), "include_published_date": true});
                if let Some(year) = args.year_from {
                    body["start_date"] = json!(format!("{year:04}-01-01"));
                }
                if let Some(year) = args.year_to {
                    body["end_date"] = json!(format!("{year:04}-12-31"));
                }
                self.client
                    .post("https://api.tavily.com/search")
                    .bearer_auth(key)
                    .json(&body)
            }
        };
        Ok(request)
    }
}

fn env_value(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    clean(&value)
}

fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or_default())
}

fn request_key(request: &Request) -> String {
    let mut hash = Sha256::new();
    hash.update(request.method().as_str());
    hash.update(b"\0");
    hash.update(request.url().as_str());
    hash.update(b"\0");
    if let Some(body) = request.body().and_then(reqwest::Body::as_bytes) {
        hash.update(body);
    }
    use std::fmt::Write as _;
    let mut key = String::with_capacity(64);
    for byte in hash.finalize() {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

async fn read_cache(path: PathBuf) -> Result<Option<Vec<u8>>> {
    tokio::task::spawn_blocking(move || match fs::read(&path) {
        Ok(body) => Ok(Some(body)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    })
    .await?
}

async fn write_raw(
    path: PathBuf,
    dump: Option<String>,
    suffix: &'static str,
    body: Vec<u8>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut file = match &dump {
            Some(prefix) => tempfile::Builder::new()
                .prefix(prefix)
                .suffix(suffix)
                .tempfile_in(&path)?,
            None => {
                tempfile::NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?
            }
        };
        file.write_all(&body)?;
        if dump.is_some() {
            file.keep().map_err(|err| err.error)?;
        } else {
            file.persist(path).map_err(|err| err.error)?;
        }
        Ok(())
    })
    .await?
}

fn date_filter(mut req: RequestBuilder, args: &SearchArgs, from: &str, to: &str) -> RequestBuilder {
    let filters: Vec<_> = [(args.year_from, from, "01-01"), (args.year_to, to, "12-31")]
        .into_iter()
        .filter_map(|(year, key, date)| year.map(|y| format!("{key}:{y:04}-{date}")))
        .collect();
    if !filters.is_empty() {
        req = req.query(&[("filter", filters.join(","))]);
    }
    req
}

fn solr_literal(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for c in query.chars() {
        if r#"+-&|!(){}[]^"~*?:\/"#.contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Accepts bare DOIs, `doi:` values and links to the DOI resolver.
pub(super) fn normalize_doi(input: &str) -> Option<String> {
    let input = input.trim();
    let lower = input.to_ascii_lowercase();
    let value = if lower.starts_with("doi:") {
        input[4..].trim().to_ascii_lowercase()
    } else if [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        let url = Url::parse(input).ok()?;
        decode_path(url.path().strip_prefix('/')?)?.to_ascii_lowercase()
    } else {
        lower
    };
    let (prefix, suffix) = value.split_once('/')?;
    let registrant = prefix.strip_prefix("10.")?;
    if registrant.len() < 4
        || !registrant
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        || suffix.is_empty()
        || suffix.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Some(value)
}

fn decode_path(path: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let hi = char::from(bytes.next()?).to_digit(16)?;
            let lo = char::from(bytes.next()?).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            byte
        });
    }
    String::from_utf8(decoded).ok()
}

fn parse(source: Source, body: &[u8], args: &SearchArgs) -> Result<Vec<Reference>> {
    let mut references = if matches!(source, Source::Arxiv) {
        parse_arxiv(body)?
    } else {
        let json: Value = serde_json::from_slice(body).context("invalid JSON response")?;
        if matches!(source, Source::ResolveDoi) {
            let item = &json["message"];
            ensure!(item.is_object(), "missing Crossref work");
            crossref(item).into_iter().collect()
        } else {
            let (rows, convert): (&Value, fn(&Value) -> Option<Reference>) = match source {
                Source::OpenAlex => (&json["results"], openalex),
                Source::Crossref => (&json["message"]["items"], crossref),
                Source::Hal => (&json["response"]["docs"], hal),
                Source::Web => (&json["results"], tavily),
                Source::ResolveDoi | Source::Arxiv => unreachable!(),
            };
            rows.as_array()
                .context("missing search results")?
                .iter()
                .filter_map(convert)
                .collect()
        }
    };
    if !matches!(source, Source::ResolveDoi) {
        references.retain(|item| {
            item.year.is_none_or(|year| {
                args.year_from.is_none_or(|from| year >= from)
                    && args.year_to.is_none_or(|to| year <= to)
            })
        });
        references.truncate(args.limit);
    }
    for reference in &mut references {
        reference.source_tool = source.name().to_owned();
    }
    Ok(references)
}

fn clean(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Array(entries) => entries.iter().filter_map(Value::as_str).find_map(clean),
        _ => value.as_str().and_then(clean),
    }
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn year(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|y| u32::try_from(y).ok())
        .filter(|y| (1..=9999).contains(y))
}

fn openalex(item: &Value) -> Option<Reference> {
    let location = &item["primary_location"];
    Some(Reference {
        title: text(&item["title"]).or_else(|| text(&item["display_name"]))?,
        authors: array(&item["authorships"])
            .iter()
            .filter_map(|a| text(&a["author"]["display_name"]))
            .collect(),
        year: year(&item["publication_year"]),
        venue: text(&location["source"]["display_name"]),
        doi: item["doi"].as_str().and_then(normalize_doi),
        citations: item["cited_by_count"].as_u64(),
        open_access: item["open_access"]["is_oa"].as_bool(),
        abstract_text: reconstruct(&item["abstract_inverted_index"]),
        url: text(&location["landing_page_url"]).or_else(|| text(&item["id"])),
        ..Reference::default()
    })
}

fn reconstruct(index: &Value) -> Option<String> {
    let mut words: Vec<_> = index
        .as_object()?
        .iter()
        .flat_map(|(word, offsets)| {
            array(offsets)
                .iter()
                .filter_map(move |offset| Some((offset.as_u64()?, word.as_str())))
        })
        .collect();
    // Sort positions rather than allocating up to the largest position from the API.
    words.sort_unstable();
    words.dedup_by_key(|item| item.0);
    clean(
        &words
            .into_iter()
            .map(|(_, word)| word)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn crossref(item: &Value) -> Option<Reference> {
    let doi = item["DOI"].as_str().and_then(normalize_doi);
    Some(Reference {
        title: text(&item["title"])?,
        authors: array(&item["author"])
            .iter()
            .filter_map(|author| {
                let name = [text(&author["given"]), text(&author["family"])]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                clean(&name).or_else(|| text(&author["name"]))
            })
            .collect(),
        year: ["published", "issued", "published-print", "published-online"]
            .into_iter()
            .find_map(|field| year(&item[field]["date-parts"][0][0])),
        venue: text(&item["container-title"]).or_else(|| text(&item["event"]["name"])),
        citations: item["is-referenced-by-count"].as_u64(),
        abstract_text: text(&item["abstract"]).map(|raw| jats_text(&raw).unwrap_or(raw)),
        url: text(&item["URL"])
            .or_else(|| doi.as_ref().map(|doi| format!("https://doi.org/{doi}"))),
        doi,
        ..Reference::default()
    })
}

fn jats_text(raw: &str) -> Option<String> {
    use quick_xml::events::Event;

    if !raw.starts_with('<') {
        return None;
    }
    let mut reader = quick_xml::Reader::from_str(raw);
    let mut text = String::new();
    loop {
        match reader.read_event().ok()? {
            Event::Text(chunk) => text.push_str(std::str::from_utf8(&chunk).ok()?),
            Event::CData(chunk) => text.push_str(std::str::from_utf8(&chunk).ok()?),
            Event::GeneralRef(reference) => {
                let entity = format!("&{};", std::str::from_utf8(&reference).ok()?);
                text.push_str(&quick_xml::escape::unescape(&entity).ok()?);
            }
            Event::End(tag) if matches!(tag.local_name().as_ref(), b"p" | b"title") => {
                text.push(' ')
            }
            Event::Eof => return Some(words(&text)),
            _ => {}
        }
    }
}

fn hal(item: &Value) -> Option<Reference> {
    let authors = &item["authFullName_s"];
    Some(Reference {
        title: text(&item["title_s"])?,
        authors: match authors {
            Value::Array(authors) => authors.iter().filter_map(text).collect(),
            _ => text(authors).into_iter().collect(),
        },
        year: year(&item["producedDateY_i"]),
        venue: ["journalTitle_s", "conferenceTitle_s", "bookTitle_s"]
            .into_iter()
            .find_map(|field| text(&item[field])),
        doi: text(&item["doiId_s"]).as_deref().and_then(normalize_doi),
        open_access: item["openAccess_bool"].as_bool(),
        abstract_text: text(&item["abstract_s"]),
        url: text(&item["uri_s"]),
        ..Reference::default()
    })
}

#[derive(Deserialize)]
enum AtomDocument {
    #[serde(rename = "feed")]
    Feed(AtomFeed),
}

#[derive(Deserialize)]
struct AtomFeed {
    #[serde(rename = "entry", default)]
    entries: Vec<AtomEntry>,
}

#[derive(Deserialize)]
struct AtomEntry {
    id: String,
    title: String,
    published: Option<String>,
    summary: Option<String>,
    doi: Option<String>,
    journal_ref: Option<String>,
    #[serde(rename = "author", default)]
    authors: Vec<AtomAuthor>,
}

#[derive(Deserialize)]
struct AtomAuthor {
    name: String,
}

fn parse_arxiv(body: &[u8]) -> Result<Vec<Reference>> {
    let AtomDocument::Feed(feed) =
        quick_xml::de::from_reader(body).context("invalid arXiv Atom response")?;
    let mut references = Vec::new();
    for entry in feed.entries {
        if entry.id.contains("/api/errors#") {
            bail!(
                "arXiv API error: {}",
                entry.summary.as_deref().unwrap_or(&entry.title)
            );
        }
        let title = words(&entry.title);
        if title.is_empty() {
            continue;
        }
        references.push(Reference {
            title,
            authors: entry
                .authors
                .into_iter()
                .filter_map(|a| clean(&a.name))
                .collect(),
            year: entry.published.as_deref().and_then(iso_year),
            venue: entry
                .journal_ref
                .as_deref()
                .and_then(clean)
                .or_else(|| Some("arXiv".to_owned())),
            doi: entry.doi.as_deref().and_then(normalize_doi),
            open_access: Some(true),
            abstract_text: entry.summary.as_deref().and_then(|s| clean(&words(s))),
            url: clean(&entry.id),
            ..Reference::default()
        });
    }
    Ok(references)
}

fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn iso_year(date: &str) -> Option<u32> {
    let (year, _) = date.split_once('-')?;
    date_year(year)
}

fn date_year(year: &str) -> Option<u32> {
    if year.len() != 4 {
        return None;
    }
    year.parse::<u32>()
        .ok()
        .filter(|&year| (1..=9999).contains(&year))
}

fn tavily(item: &Value) -> Option<Reference> {
    let url = text(&item["url"]);
    Some(Reference {
        title: text(&item["title"])?,
        year: item["published_date"]
            .as_str()
            .and_then(|date| iso_year(date).or_else(|| date_year(date.split_whitespace().nth(3)?))),
        doi: url.as_deref().and_then(normalize_doi),
        abstract_text: text(&item["content"]),
        url,
        ..Reference::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENALEX: &[u8] = include_bytes!("../tests/fixtures/biblio_openalex.json");

    fn args() -> SearchArgs {
        SearchArgs {
            query: "façade & humidité".to_owned(),
            year_from: None,
            year_to: None,
            limit: 10,
        }
    }

    #[test]
    fn maps_and_filters_openalex() -> Result<()> {
        let args = SearchArgs {
            year_from: Some(2020),
            year_to: Some(2021),
            ..args()
        };
        let results = parse(Source::OpenAlex, OPENALEX, &args)?;
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.title, "Envelope performance in winter");
        assert_eq!(r.authors, ["Camille Durand", "Ada Lee"]);
        assert_eq!(r.year, Some(2021));
        assert_eq!(r.venue.as_deref(), Some("Building Physics"));
        assert_eq!(r.doi.as_deref(), Some("10.1234/example.a"));
        assert_eq!(r.citations, Some(11));
        assert_eq!(r.open_access, Some(false));
        assert_eq!(
            r.abstract_text.as_deref(),
            Some("We study the façade study")
        );
        assert_eq!(r.url.as_deref(), Some("https://example.org/article"));
        assert_eq!(r.source_tool, "search_openalex");
        Ok(())
    }

    #[test]
    fn reconstructs_sparse_and_repeated_words_without_padding() {
        let index = json!({"end.": [999999999999_u64], "repeat": [2, 0], "ignored": [-1, "3"]});
        assert_eq!(reconstruct(&index).as_deref(), Some("repeat repeat end."));
        assert_eq!(reconstruct(&json!({})), None);
        assert_eq!(reconstruct(&Value::Null), None);
    }

    #[test]
    fn maps_crossref_search() -> Result<()> {
        let results = parse(
            Source::Crossref,
            include_bytes!("../tests/fixtures/biblio_crossref_search.json"),
            &args(),
        )?;
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.title, "Coupled heat and moisture transfer");
        assert_eq!(r.authors, ["Ada Lee", "Building Science Group"]);
        assert_eq!(r.year, Some(2022));
        assert_eq!(r.citations, Some(0));
        assert_eq!(r.open_access, None);
        assert_eq!(r.doi.as_deref(), Some("10.2345/building.1"));
        assert_eq!(r.source_tool, "search_crossref");
        Ok(())
    }

    #[test]
    fn resolves_one_crossref_work_without_search_filters() -> Result<()> {
        let args = SearchArgs {
            query: "10.3456/Meeting.A".to_owned(),
            year_from: Some(2020),
            limit: 0,
            ..args()
        };
        let results = parse(
            Source::ResolveDoi,
            include_bytes!("../tests/fixtures/biblio_crossref_resolve.json"),
            &args,
        )?;
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.authors, ["Durand"]);
        assert_eq!(r.year, Some(2018));
        assert_eq!(r.venue.as_deref(), Some("Thermal Simulation Conference"));
        assert_eq!(r.abstract_text.as_deref(), Some("A sensitivity study."));
        assert_eq!(r.url.as_deref(), Some("https://doi.org/10.3456/meeting.a"));
        assert_eq!(r.source_tool, "resolve_doi");
        Ok(())
    }

    #[test]
    fn maps_hal() -> Result<()> {
        let results = parse(
            Source::Hal,
            include_bytes!("../tests/fixtures/biblio_hal.json"),
            &args(),
        )?;
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.title, "Isolation thermique et humidité");
        assert_eq!(r.authors, ["Camille Durand", "Lou Bernard"]);
        assert_eq!(r.year, Some(2023));
        assert_eq!(r.venue.as_deref(), Some("Journées de thermique"));
        assert_eq!(r.doi.as_deref(), Some("10.4567/example.hal"));
        assert_eq!(r.open_access, Some(true));
        assert_eq!(
            r.abstract_text.as_deref(),
            Some("Étude du transfert de chaleur.")
        );
        assert_eq!(r.source_tool, "search_hal");
        Ok(())
    }

    #[test]
    fn maps_atom_and_namespaced_arxiv_fields() -> Result<()> {
        let results = parse(
            Source::Arxiv,
            include_bytes!("../tests/fixtures/biblio_arxiv.xml"),
            &args(),
        )?;
        assert_eq!(results.len(), 2);
        let r = &results[0];
        assert_eq!(r.title, "Temperature & humidity in a test cell");
        assert_eq!(r.authors, ["Camille Durand", "Ada Lee"]);
        assert_eq!(r.year, Some(2024));
        assert_eq!(r.venue.as_deref(), Some("Journal of Heat Transfer"));
        assert_eq!(r.doi.as_deref(), Some("10.5678/example.arxiv"));
        assert_eq!(r.open_access, Some(true));
        assert_eq!(
            r.abstract_text.as_deref(),
            Some("We model the envelope. We compare the results.")
        );
        assert_eq!(r.source_tool, "search_arxiv");
        assert_eq!(results[1].venue.as_deref(), Some("arXiv"));
        assert_eq!(results[1].doi, None);
        assert!(parse_arxiv(b"<html><body>not Atom</body></html>").is_err());
        Ok(())
    }

    #[test]
    fn canonicalizes_dois() {
        for value in [
            " DOI: 10.1234/A/B ",
            "HTTPS://DX.DOI.ORG/10.1234%2FA%2FB?x=y#z",
            "10.1234/a/b",
        ] {
            assert_eq!(
                normalize_doi(value).as_deref(),
                Some("10.1234/a/b"),
                "{value}"
            );
        }
        for value in [
            "arXiv:2401.01234",
            "10.1234/",
            "10.abcd/x",
            "10.1234/a b",
            "https://doi.org/10.1234/a%ZZ",
        ] {
            assert_eq!(normalize_doi(value), None, "{value}");
        }
    }

    #[test]
    fn encodes_parameters_and_uses_contact_details() -> Result<()> {
        let mut sources = Sources::new(None, None)?;
        sources.openalex_mailto = Some("test+openalex@example.org".to_owned());
        sources.crossref_agent = format!("{USER_AGENT} (mailto:test@example.org)");
        let args = SearchArgs {
            year_from: Some(2020),
            year_to: Some(2024),
            ..args()
        };
        let openalex = sources.request(Source::OpenAlex, &args)?.build()?;
        let pairs: std::collections::HashMap<_, _> = openalex.url().query_pairs().collect();
        assert_eq!(pairs["search"], args.query);
        assert_eq!(pairs["mailto"], "test+openalex@example.org");
        assert!(!pairs.contains_key("api_key"));
        assert_eq!(
            pairs["filter"],
            "from_publication_date:2020-01-01,to_publication_date:2024-12-31"
        );
        let crossref = sources.request(Source::Crossref, &args)?.build()?;
        assert_eq!(
            crossref.headers()[reqwest::header::USER_AGENT],
            sources.crossref_agent
        );
        let pairs: std::collections::HashMap<_, _> = crossref.url().query_pairs().collect();
        assert_eq!(pairs["query.bibliographic"], args.query);
        assert_eq!(
            pairs["filter"],
            "from-pub-date:2020-01-01,until-pub-date:2024-12-31"
        );
        let args = SearchArgs {
            query: "10.1234/A?B#C".to_owned(),
            ..args
        };
        let resolve = sources.request(Source::ResolveDoi, &args)?.build()?;
        assert_eq!(resolve.url().path(), "/works/10.1234%2Fa%3Fb%23c");
        assert_eq!(resolve.url().query(), None);
        assert_eq!(resolve.url().fragment(), None);
        Ok(())
    }

    #[test]
    fn tavily_is_optional_and_keeps_the_key_out_of_the_body() -> Result<()> {
        let mut sources = Sources::new(None, None)?;
        let args = args();
        sources.tavily_key = None;
        assert!(!sources.web_enabled());
        assert!(sources.request(Source::Web, &args).is_err());
        sources.tavily_key = Some("test-key".to_owned());
        assert!(sources.web_enabled());
        let request = sources.request(Source::Web, &args)?.build()?;
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer test-key"
        );
        let body: Value = serde_json::from_slice(
            request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .context("missing JSON body")?,
        )?;
        assert_eq!(body["query"], args.query);
        assert!(body.get("api_key").is_none());
        let result = tavily(
            &json!({"title": "Web paper", "published_date": "Tue, 11 Mar 2025 17:00:00 GMT"}),
        )
        .context("missing result")?;
        assert_eq!(result.year, Some(2025));
        Ok(())
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates() -> Result<()> {
        let date = "Wed, 21 Oct 2015 07:28:00 GMT";
        let now = httpdate::parse_http_date(date)? - Duration::from_secs(3);
        assert_eq!(retry_after(" 5 ", now), Some(Duration::from_secs(5)));
        assert_eq!(retry_after(date, now), Some(Duration::from_secs(3)));
        assert_eq!(
            retry_after("18446744073709551616", now),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(retry_after("invalid", now), None);
        Ok(())
    }

    #[tokio::test]
    async fn reuses_disk_cache_and_dumps_original_bytes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = temp.path().join("cache");
        let dump = temp.path().join("dump");
        let sources = Sources::new(Some(cache.clone()), Some(dump.clone()))?;
        let args = args();
        let source = Source::OpenAlex;
        let request = sources.request(source, &args)?.build()?;
        let path = cache.join(format!(
            "{}-{}{}",
            source.name(),
            request_key(&request),
            source.extension()
        ));
        write_raw(path, None, source.extension(), OPENALEX.to_vec()).await?;
        let results = sources.call("test", source, args).await?;
        assert_eq!(results.len(), 2);
        let files: Vec<_> = fs::read_dir(dump)?.collect::<io::Result<_>>()?;
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read(files[0].path())?, OPENALEX);
        Ok(())
    }
}
