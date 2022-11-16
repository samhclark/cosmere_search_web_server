use std::{
    collections::HashMap,
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
};

use axum::{
    extract::Query,
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, get_service},
    Router,
};
use domain::{HtmlTemplate, IndexableBook, ResultsTemplate, RichParagraph};
use futures::future;
use html2text::from_read;
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response,
};
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use tantivy::{
    collector::TopDocs,
    doc,
    query::QueryParser,
    schema::{IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED, TEXT},
    DocAddress, Index, Score,
};
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};
use tracing::Level;

mod books;
mod domain;

type Label = (String, String);

#[allow(unused_must_use)]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();
    let mut registry = <Registry>::with_prefix("csearch");
    let http_requests = Family::<Label, Counter>::default();
    registry.register(
        "http_requests",
        "Number of HTTP requests received",
        Box::new(http_requests.clone()),
    );

    let books = books::load_all();
    let tantivy_index = build_search_index();
    for book in books {
        add_book(book, &tantivy_index);
    }

    // Create application server
    let app = Router::new()
        .fallback(get_service(ServeDir::new("./assets")).handle_error(handle_error))
        .route("/search", get(|q| search(q, tantivy_index, http_requests)))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; img-src 'self'; script-src 'self'; style-src 'self'",
            ),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=63072000"),
        ));

    let app_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8080);    
    let app_server =
        axum::Server::bind(&app_addr).serve(app.into_make_service());
    tracing::info!("Application listening on {app_addr}");

    // Create metrics server
    let metrics_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 9091);
    let arc_registry = Arc::new(registry);
    let metrics_server = axum::Server::bind(&metrics_addr).serve(make_service_fn(move |_conn| {
        let registry = arc_registry.clone();
        async move {
            let handler = make_handler(registry);
            Ok::<_, io::Error>(service_fn(handler))
        }
    }));
    tracing::info!("Metrics server listening on {metrics_addr}");

    // Start both servers
    future::join(app_server, metrics_server).await;
}

/// This function returns a HTTP handler (i.e. another function)
pub fn make_handler(
    registry: Arc<Registry>,
) -> impl Fn(Request<Body>) -> Pin<Box<dyn Future<Output = io::Result<Response<Body>>> + Send>> {
    // This closure accepts a request and responds with the OpenMetrics encoding of the metrics.
    move |_req: Request<Body>| {
        let reg = registry.clone();
        Box::pin(async move {
            let mut buf = Vec::new();
            encode(&mut buf, &reg.clone()).map(|_| {
                let body = Body::from(buf);
                Response::builder()
                    .header(
                        hyper::header::CONTENT_TYPE,
                        "application/openmetrics-text; version=1.0.0; charset=utf-8",
                    )
                    .body(body)
                    .unwrap()
            })
        })
    }
}

#[allow(clippy::unused_async)]
async fn handle_error(_err: io::Error) -> impl IntoResponse {
    (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong...")
}

#[allow(clippy::unused_async)]
async fn search(
    Query(params): Query<HashMap<String, String>>,
    index: Index,
    http_requests: Family<Label, Counter>,
) -> impl IntoResponse {
    http_requests
        .get_or_create(&(String::from("GET"), String::from("/search")))
        .inc();
    let search_term: String = params
        .get("q")
        .unwrap()
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c == &' ')
        .collect();
    tracing::info!("Searched for \"{}\"", search_term);
    let reader = index.reader().unwrap();

    let searcher = reader.searcher();

    let book_field = index.schema().get_field("book_title").unwrap();
    let chapter_field = index.schema().get_field("chapter_title").unwrap();
    let paragraph_field = index.schema().get_field("paragraph").unwrap();
    let query_parser = QueryParser::for_index(&index, vec![paragraph_field]);

    // QueryParser may fail if the query is not in the right format
    // TODO: toss up a 400 Bad Request when that happens
    let query = query_parser.parse_query(&search_term).unwrap();

    let top_docs: Vec<(Score, DocAddress)> =
        searcher.search(&query, &TopDocs::with_limit(20)).unwrap();

    let mut results: Vec<RichParagraph> = vec![];
    for (_score, doc_address) in top_docs {
        let retrieved_doc = searcher.doc(doc_address).unwrap();
        results.push(RichParagraph {
            book: retrieved_doc
                .get_first(book_field)
                .unwrap()
                .as_text()
                .unwrap()
                .to_string(),
            chapter: retrieved_doc
                .get_first(chapter_field)
                .unwrap()
                .as_text()
                .unwrap()
                .to_string(),
            text: retrieved_doc
                .get_first(paragraph_field)
                .unwrap()
                .as_text()
                .unwrap()
                .to_string(),
        });
    }

    let template = ResultsTemplate {
        search_term: search_term.clone(),
        search_results: results,
    };
    HtmlTemplate(template)
}

#[allow(dead_code)]
/// My own method for helping "look at" a book that I'm trying to load
fn inspect(book: &IndexableBook) {
    println!("Spine: ");
    for (i, s) in book.epub_file.spine.iter().enumerate() {
        println!("{}\t{}", i, s);
    }
}

fn build_search_index() -> Index {
    let mut schema_builder = Schema::builder();

    let text_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("en_stem")
                .set_index_option(IndexRecordOption::Basic),
        )
        .set_stored();

    schema_builder.add_text_field("book_title", TEXT | STORED);
    schema_builder.add_text_field("chapter_title", TEXT | STORED);
    schema_builder.add_text_field("paragraph", text_options);
    let schema = schema_builder.build();

    Index::create_from_tempdir(schema).expect("Building index in tempdir should not fail")
}

fn add_book(book: IndexableBook, index: &Index) {
    let mut index_writer = index.writer(128_000_000).unwrap();

    let book_field = index.schema().get_field("book_title").unwrap();
    let chapter_field = index.schema().get_field("chapter_title").unwrap();
    let paragraph_field = index.schema().get_field("paragraph").unwrap();

    let mut doc = book.epub_file;

    for i in book.first_chapter_index..=book.last_chapter_index {
        if book.skippable_chapters.contains(&i) {
            continue;
        }
        doc.set_current_page(i)
            .expect("Indexes used in `skippable_chapters` must be valid");
        let chapter_title = doc.spine[i].clone();
        let this_page = doc.get_current().unwrap();
        let page_content = from_read(&this_page[..], usize::MAX);
        for line in page_content.lines() {
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                continue;
            }
            index_writer
                .add_document(doc!(
                    book_field => book.title.clone(),
                    chapter_field => pretty_chapter(&chapter_title),
                    paragraph_field => line))
                .unwrap();
        }
    }

    index_writer.commit().unwrap();
}

fn pretty_chapter(raw_chapter: &str) -> String {
    if raw_chapter.to_ascii_lowercase() == "prologue" {
        String::from("Prologue")
    } else if raw_chapter.to_ascii_lowercase() == "epilogue" {
        String::from("Epilogue")
    } else if raw_chapter.to_ascii_lowercase().starts_with("chapter") {
        let num: String = raw_chapter
            .chars()
            .into_iter()
            .filter(char::is_ascii_digit)
            .collect();
        format!("Chapter {num}")
    } else {
        String::from(raw_chapter)
    }
}
