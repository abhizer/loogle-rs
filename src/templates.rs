use askama::Template;

use crate::model::QueryResult;

#[derive(Template)]
#[template(path = "results.html")]
pub struct Results {
    pub search_word: String,
    pub results: QueryResult,
}
