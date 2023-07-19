use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Seek},
    path::{Path, PathBuf},
};

use async_compression::{tokio::bufread::ZstdDecoder, tokio::write::ZstdEncoder, Level};
use color_eyre::eyre::Result;
use notify::Event;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tracing::instrument;
use walkdir::DirEntry;

pub type TermFerq = HashMap<String, usize>;
pub type DocFreq = HashMap<String, usize>;

pub type Docs = HashMap<PathBuf, Document>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    tf: TermFerq,
    terms: usize,
    hash: String,
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub result: Vec<Record>,
}

impl From<Vec<Record>> for QueryResult {
    fn from(value: Vec<Record>) -> Self {
        QueryResult { result: value }
    }
}

#[derive(Debug, Serialize)]
pub struct Record {
    pub path: PathBuf,
    pub score: f32,
}

impl Record {
    pub fn new(path: PathBuf, score: f32) -> Self {
        Record { path, score }
    }
}

trait Tokenize {
    fn tokenize(&self) -> Vec<String>;
}

impl Tokenize for &str {
    fn tokenize(&self) -> Vec<String> {
        self.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .map(|w| w.to_lowercase())
            .collect()
    }
}

impl Document {
    pub fn new(path: &Path) -> Result<Document> {
        let mut tf = TermFerq::new();

        let file = File::open(path)?;
        let mut buf_reader = BufReader::new(file);

        let mut buf = String::new();

        while let Ok(read) = buf_reader.read_line(&mut buf) {
            if read == 0 {
                break;
            }

            buf.as_str().tokenize().into_iter().for_each(|word| {
                tf.entry(word).and_modify(|c| *c += 1).or_insert(1);
            });
        }

        let terms = tf.len();
        let hash = Self::hash(&mut buf_reader)?;

        Ok(Document { tf, terms, hash })
    }

    fn hash(buf_reader: &mut BufReader<File>) -> Result<String> {
        buf_reader.seek(std::io::SeekFrom::Start(0))?;
        let mut hasher: Sha3_256 = Digest::new();

        std::io::copy(buf_reader, &mut hasher)?;

        Ok(format!("{:02x}", hasher.finalize()))
    }

    pub fn requires_reindexing(&mut self, path: &Path) -> bool {
        File::open(path)
            .map(|f| {
                let mut buf_reader = BufReader::new(f);

                Self::hash(&mut buf_reader)
                    .map(|h| h != self.hash)
                    .unwrap_or(true)
            })
            .unwrap_or(true)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Model {
    docs: Docs,
    df: DocFreq,
    index: PathBuf,
}

impl Model {
    #[instrument(skip(self), err)]
    pub fn add_document_from_path(&mut self, path: PathBuf) -> Result<()> {
        let doc = Document::new(&path)?;

        self.add_document(doc, path)
    }

    #[instrument(skip(self, doc), err)]
    pub fn add_document(&mut self, doc: Document, path: PathBuf) -> Result<()> {
        tracing::info!("adding document: {}", path.display());
        if self
            .docs
            .get(&path)
            .map(|d| d.hash != doc.hash)
            .unwrap_or(false)
        {
            self.remove_document(&path);
        }

        doc.tf.keys().for_each(|k| {
            if let Some(v) = self.df.get_mut(k) {
                *v += 1;
            } else {
                self.df.insert(k.to_owned(), 1);
            }
        });

        self.docs.insert(path, doc);

        Ok(())
    }

    #[instrument(skip(self))]
    pub fn remove_document(&mut self, path: &Path) {
        if let Some(doc) = self.docs.remove(path) {
            doc.tf.keys().for_each(|k| {
                if let Some(v) = self.df.get_mut(k.as_str()) {
                    *v -= 1;
                }
            });
        }
    }

    #[instrument(skip(self))]
    pub fn remove_directory(&mut self, dir_path: &Path) {
        let to_remove: Vec<PathBuf> = self
            .docs
            .keys()
            .cloned()
            .filter(|k| k.starts_with(dir_path))
            .collect();

        to_remove
            .into_iter()
            .for_each(|doc| self.remove_document(&doc));
    }

    #[instrument(skip(self))]
    pub fn query(&self, query: &str) -> QueryResult {
        let mut results = Vec::new();

        let tokens = query.tokenize();

        self.docs.iter().for_each(|(path, doc)| {
            let mut rank = 0f32;
            for token in &tokens {
                rank +=
                    compute_tf(token, doc) * compute_idf(token, self.docs.len() as f32, &self.df);
            }
            if !rank.is_nan() && rank != 0.0 {
                results.push(Record::new(path.clone(), rank));
            }
        });

        results.sort_by(
            |Record { score: score1, .. }, Record { score: score2, .. }| {
                score1.partial_cmp(score2).unwrap()
            },
        );
        results.reverse();

        results.into()
    }

    #[instrument(err)]
    pub async fn load_from_index(index: &Path) -> Result<Self> {
        let file = tokio::fs::File::open(index).await?;
        let buf_reader = tokio::io::BufReader::new(file);

        let mut decoder = ZstdDecoder::new(buf_reader);
        let mut buf = String::new();

        decoder.read_to_string(&mut buf).await?;

        Ok(serde_json::from_str(&buf)?)
    }

    #[instrument]
    pub fn from_data_dir(index: &Path, data_dir: &Path) -> Self {
        fn is_hidden(entry: &DirEntry) -> bool {
            entry
                .file_name()
                .to_str()
                .map(|s| s.starts_with('.'))
                .unwrap_or(false)
        }

        let mut model = Model {
            index: index.to_path_buf(),
            ..Default::default()
        };

        let walker = walkdir::WalkDir::new(data_dir).into_iter();
        let dir_entries = walker.filter_entry(|e| !is_hidden(e));

        let docs: Vec<(Document, PathBuf)> = dir_entries
            .par_bridge()
            .into_par_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                Document::new(e.path())
                    .ok()
                    .map(|d| (d, e.path().to_path_buf()))
            })
            .collect();

        docs.into_iter().for_each(|(d, p)| {
            _ = model.add_document(d, p);
        });

        model
    }

    #[instrument(err)]
    pub async fn load_index_or_from_data_dir(index: &Path, data_dir: &Path) -> Result<Self> {
        Ok(Self::load_from_index(index)
            .await
            .unwrap_or_else(|_| Self::from_data_dir(index, data_dir)))
    }

    #[instrument(skip(self), err)]
    pub async fn save(&self) -> Result<()> {
        tracing::info!("saving model to {}", self.index.display());

        let file = tokio::fs::File::create(&self.index).await?;
        let mut encoder = ZstdEncoder::with_quality(file, Level::Best);

        let json = serde_json::to_string(self)?;
        encoder.write_all(json.as_bytes()).await?;

        encoder.shutdown().await?;

        Ok(())
    }

    pub fn update(&mut self, event: Event) -> bool {
        match event.kind {
            notify::EventKind::Create(notify::event::CreateKind::File) => {
                event.paths.into_iter().for_each(|path| {
                    _ = self.add_document_from_path(path);
                });
                true
            }
            notify::EventKind::Modify(notify::event::ModifyKind::Data(_)) => {
                for path in event.paths.into_iter() {
                    _ = self.add_document_from_path(path);
                }
                true
            }
            notify::EventKind::Remove(rm_kind) => match rm_kind {
                notify::event::RemoveKind::File => {
                    event.paths.iter().for_each(|path| {
                        self.remove_document(path);
                    });
                    true
                }
                notify::event::RemoveKind::Folder => {
                    event
                        .paths
                        .iter()
                        .for_each(|path| self.remove_directory(path));
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }
}

fn compute_tf(token: &str, doc: &Document) -> f32 {
    let n = doc.terms as f32;
    let m = doc.tf.get(token).cloned().unwrap_or(0) as f32;

    m / n
}

fn compute_idf(token: &str, n: f32, df: &DocFreq) -> f32 {
    let m = df.get(token).cloned().unwrap_or(1) as f32;
    (n / m).log10()
}
