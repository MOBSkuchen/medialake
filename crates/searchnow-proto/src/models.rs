//! Built-in model manifest: which GGUF files make up each quality tier.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Heavy,
    #[default]
    Normal,
    Light,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Heavy, Quality::Normal, Quality::Light];

    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Heavy => "heavy",
            Quality::Normal => "normal",
            Quality::Light => "light",
        }
    }

    pub fn parse(s: &str) -> Option<Quality> {
        match s.to_ascii_lowercase().as_str() {
            "heavy" => Some(Quality::Heavy),
            "normal" => Some(Quality::Normal),
            "light" => Some(Quality::Light),
            _ => None,
        }
    }
}

pub const MODEL_ID: &str = "embeddinggemma-2";

#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelSet {
    pub quality: Quality,
    pub model: ModelFile,
    pub mmproj: ModelFile,
}

impl ModelSet {
    pub fn total_size(&self) -> u64 {
        self.model.size + self.mmproj.size
    }
}

const GGML: &str = "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/";
const UNSLOTH: &str = "https://huggingface.co/unsloth/embeddinggemma-2-GGUF/resolve/main/";

const MMPROJ_Q8: ModelFile = ModelFile {
    file_name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    url: "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/mmproj-embeddinggemma-2-Q8_0.gguf",
    sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
    size: 554_821_024,
};

pub fn model_set(q: Quality) -> ModelSet {
    let _ = (GGML, UNSLOTH);
    match q {
        Quality::Heavy => ModelSet {
            quality: q,
            model: ModelFile {
                file_name: "embeddinggemma-2-BF16.gguf",
                url: "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-BF16.gguf",
                sha256: "68bae29d62fb8c7d23e98d21fd4662753ddd636e6b62b8a70dfe059a9844f216",
                size: 557_950_176,
            },
            mmproj: ModelFile {
                file_name: "mmproj-embeddinggemma-2-BF16.gguf",
                url: "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/mmproj-embeddinggemma-2-BF16.gguf",
                sha256: "d2033b3cd0223cfb2e7a2b50dc80a664c78c816d375f5b017ec3ef80ee0bc766",
                size: 982_074_784,
            },
        },
        Quality::Normal => ModelSet {
            quality: q,
            model: ModelFile {
                file_name: "embeddinggemma-2-Q8_0.gguf",
                url: "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-Q8_0.gguf",
                sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
                size: 309_855_456,
            },
            mmproj: MMPROJ_Q8,
        },
        Quality::Light => ModelSet {
            quality: q,
            model: ModelFile {
                file_name: "embeddinggemma-2-UD-Q4_K_XL.gguf",
                url: "https://huggingface.co/unsloth/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-UD-Q4_K_XL.gguf",
                sha256: "ea905fd08e8061db77a0031cbb5096d459d7cfd2bdd0ad2ba66e483f16719493",
                size: 175_673_856,
            },
            mmproj: MMPROJ_Q8,
        },
    }
}
