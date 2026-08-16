use std::path::{Path, PathBuf};

use model2vec_rs::model::StaticModel;

/// Model name that selects the model compiled into the binary.
pub const DEFAULT_MODEL: &str = "default";

/// The model vendored under `src/embedding/` and baked into the binary by
/// `include_bytes!`. 131 MB of it, so it is committed to git deliberately —
/// see AGENTS.md before adding a second one.
const EMBEDDED_MODEL_NAME: &str = "potion-retrieval-32M";
const EMBEDDED_TOKENIZER: &[u8] = include_bytes!("potion-retrieval-32M/tokenizer.json");
const EMBEDDED_WEIGHTS: &[u8] = include_bytes!("potion-retrieval-32M/model.safetensors");
const EMBEDDED_CONFIG: &[u8] = include_bytes!("potion-retrieval-32M/config.json");

/// Files a model directory must contain, in the order they are reported missing.
const REQUIRED_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Embedding model path {path:?} is not a directory")]
    NotADirectory { path: PathBuf },

    #[error("Embedding model directory {path:?} is missing {file}")]
    MissingFile { path: PathBuf, file: &'static str },

    #[error("Could not load embedding model from directory {path:?}")]
    LoadFromDirectory {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("Could not load the built-in embedding model {name:?}")]
    LoadEmbedded {
        name: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

pub trait Embedding: Send + Sync {
    /// Name of the loaded model, for logs and CLI output.
    fn name(&self) -> &str;

    /// Width of the vectors this model produces. Fixed for the life of the
    /// model, and the LanceDB schema depends on it.
    fn dimensions(&self) -> usize;

    /// Embed a batch of texts. Returns one vector per input, in order.
    ///
    /// Infallible: `model2vec` is a lookup table plus pooling, with no
    /// inference that can fail. Everything that can go wrong happens at load
    /// time, which is where [`Error`] lives.
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>>;
}

/// Model compiled into the binary. Needs no files on disk and cannot be
/// missing at runtime.
pub struct Embedded {
    model: StaticModel,
    dimensions: usize,
}

impl Embedded {
    pub fn new() -> Result<Self, Error> {
        let model = match StaticModel::from_bytes(
            EMBEDDED_TOKENIZER,
            EMBEDDED_WEIGHTS,
            EMBEDDED_CONFIG,
            None,
        ) {
            Ok(model) => model,
            Err(source) => {
                return Err(Error::LoadEmbedded {
                    name: EMBEDDED_MODEL_NAME,
                    source: source.into(),
                });
            }
        };
        let dimensions = probe_dimensions(&model);
        Ok(Self { model, dimensions })
    }
}

impl Embedding for Embedded {
    fn name(&self) -> &str {
        EMBEDDED_MODEL_NAME
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.model.encode(texts)
    }
}

/// Model loaded from a directory holding [`REQUIRED_FILES`].
pub struct Directory {
    model: StaticModel,
    name: String,
    dimensions: usize,
}

impl Directory {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Err(Error::NotADirectory {
                path: path.to_path_buf(),
            });
        }
        // model2vec-rs collapses "no such directory", "no config.json" and
        // "no safetensors" into one opaque message, so name the missing file
        // here instead.
        for file in REQUIRED_FILES {
            if !path.join(file).is_file() {
                return Err(Error::MissingFile {
                    path: path.to_path_buf(),
                    file,
                });
            }
        }
        let model = match StaticModel::from_pretrained(path, None, None, None) {
            Ok(model) => model,
            Err(source) => {
                return Err(Error::LoadFromDirectory {
                    path: path.to_path_buf(),
                    source: source.into(),
                });
            }
        };
        let name = match path.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            // A path ending in `..`, or the root; neither has a file name.
            None => path.display().to_string(),
        };
        let dimensions = probe_dimensions(&model);
        Ok(Self {
            model,
            name,
            dimensions,
        })
    }
}

impl Embedding for Directory {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.model.encode(texts)
    }
}

/// Resolve a model name to a loaded embedder.
///
/// [`DEFAULT_MODEL`] selects the model built into the binary; any other value
/// is treated as the path of a model directory.
pub fn load(name: &str) -> Result<Box<dyn Embedding>, Error> {
    if name == DEFAULT_MODEL {
        Ok(Box::new(Embedded::new()?))
    } else {
        Ok(Box::new(Directory::new(name)?))
    }
}

/// `StaticModel` keeps its shape private and exposes no accessor for the
/// embedding width, so the only way to learn it is to embed something and
/// measure the result. Done once at load time and cached on the struct.
fn probe_dimensions(model: &StaticModel) -> usize {
    model.encode_single("a").len()
}
