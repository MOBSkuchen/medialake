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

/// Pooling of the token states into the sequence embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    Mean,
    Last,
}

/// Raw cosine -> confidence mapping that fits a model (see docs/calibration.md).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    pub score_floor: f32,
    pub score_ceiling: f32,
    pub relative_cut: f32,
}

/// A supported embedding model. Vectors of different models (or qualities) are not comparable.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Model {
    #[default]
    #[serde(rename = "embeddinggemma-2")]
    EmbeddingGemma2,
    #[serde(rename = "qwen3-vl-embedding-2b")]
    Qwen3Vl2b,
    #[serde(rename = "qwen3-vl-embedding-8b")]
    Qwen3Vl8b,
}

impl Model {
    pub const ALL: [Model; 3] = [Model::EmbeddingGemma2, Model::Qwen3Vl2b, Model::Qwen3Vl8b];

    pub fn as_str(self) -> &'static str {
        match self {
            Model::EmbeddingGemma2 => "embeddinggemma-2",
            Model::Qwen3Vl2b => "qwen3-vl-embedding-2b",
            Model::Qwen3Vl8b => "qwen3-vl-embedding-8b",
        }
    }

    pub fn parse(s: &str) -> Option<Model> {
        let s = s.to_ascii_lowercase();
        Model::ALL.into_iter().find(|m| m.as_str() == s)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Model::EmbeddingGemma2 => "EmbeddingGemma 2",
            Model::Qwen3Vl2b => "Qwen3-VL-Embedding 2B",
            Model::Qwen3Vl8b => "Qwen3-VL-Embedding 8B",
        }
    }

    /// Embedding dimensionality (full size, no Matryoshka truncation).
    pub fn dims(self) -> usize {
        match self {
            Model::EmbeddingGemma2 => 768,
            Model::Qwen3Vl2b => 2048,
            Model::Qwen3Vl8b => 4096,
        }
    }

    pub fn pooling(self) -> Pooling {
        match self {
            Model::EmbeddingGemma2 => Pooling::Mean,
            Model::Qwen3Vl2b | Model::Qwen3Vl8b => Pooling::Last,
        }
    }

    /// Whether the projector has an audio encoder (the Qwen3-VL models are text / image / video only).
    pub fn supports_audio(self) -> bool {
        matches!(self, Model::EmbeddingGemma2)
    }

    pub fn calibration(self) -> Calibration {
        match self {
            Model::EmbeddingGemma2 => Calibration { score_floor: 0.58, score_ceiling: 0.74, relative_cut: 0.06 },
            // measured with 2B / normal only; 8B uses the same mapping until it is measured
            Model::Qwen3Vl2b | Model::Qwen3Vl8b => Calibration { score_floor: 0.38, score_ceiling: 0.60, relative_cut: 0.12 },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelSet {
    pub model_id: Model,
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

pub fn model_set(m: Model, q: Quality) -> ModelSet {
    match m {
        Model::EmbeddingGemma2 => gemma_set(q),
        Model::Qwen3Vl2b => qwen_2b_set(q),
        Model::Qwen3Vl8b => qwen_8b_set(q),
    }
}

fn gemma_set(q: Quality) -> ModelSet {
    let _ = (GGML, UNSLOTH);
    match q {
        Quality::Heavy => ModelSet {
            model_id: Model::EmbeddingGemma2,
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
            model_id: Model::EmbeddingGemma2,
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
            model_id: Model::EmbeddingGemma2,
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

// Qwen publishes no official GGUF files: community conversions (DevQuasar for 2B, mradermacher for 8B).

const QWEN_2B_MMPROJ_F16: ModelFile = ModelFile {
    file_name: "mmproj-Qwen.Qwen3-VL-Embedding-2B.f16.gguf",
    url: "https://huggingface.co/DevQuasar/Qwen.Qwen3-VL-Embedding-2B-GGUF/resolve/main/mmproj-Qwen.Qwen3-VL-Embedding-2B.f16.gguf",
    sha256: "3f89a7768ffa6606935319f71bf56bb71871249ba549bf1080a0caea7a088613",
    size: 819_395_136,
};

fn qwen_2b_set(q: Quality) -> ModelSet {
    let model = match q {
        Quality::Heavy => ModelFile {
            file_name: "Qwen.Qwen3-VL-Embedding-2B.f16.gguf",
            url: "https://huggingface.co/DevQuasar/Qwen.Qwen3-VL-Embedding-2B-GGUF/resolve/main/Qwen.Qwen3-VL-Embedding-2B.f16.gguf",
            sha256: "704a96b2c8a73c73c92a8fb5c70996140883e517fffcadc56c16495d628cfcdb",
            size: 3_447_350_880,
        },
        Quality::Normal => ModelFile {
            file_name: "Qwen.Qwen3-VL-Embedding-2B.Q8_0.gguf",
            url: "https://huggingface.co/DevQuasar/Qwen.Qwen3-VL-Embedding-2B-GGUF/resolve/main/Qwen.Qwen3-VL-Embedding-2B.Q8_0.gguf",
            sha256: "7552c2f699c546ce46abd6b66b2aa16ae667c88c830efbd352b12224d4613492",
            size: 1_834_428_000,
        },
        Quality::Light => ModelFile {
            file_name: "Qwen.Qwen3-VL-Embedding-2B.Q4_K_M.gguf",
            url: "https://huggingface.co/DevQuasar/Qwen.Qwen3-VL-Embedding-2B-GGUF/resolve/main/Qwen.Qwen3-VL-Embedding-2B.Q4_K_M.gguf",
            sha256: "42a4ebc629ecc6514649e12b1529b857f54900273bb854f853c970fb90edd09d",
            size: 1_107_410_528,
        },
    };
    ModelSet { model_id: Model::Qwen3Vl2b, quality: q, model, mmproj: QWEN_2B_MMPROJ_F16 }
}

const QWEN_8B_MMPROJ_F16: ModelFile = ModelFile {
    file_name: "Qwen3-VL-Embedding-8B.mmproj-f16.gguf",
    url: "https://huggingface.co/mradermacher/Qwen3-VL-Embedding-8B-GGUF/resolve/main/Qwen3-VL-Embedding-8B.mmproj-f16.gguf",
    sha256: "c507828405f645670c829be93fa57fb890af5b7abbe2583435f4a8042d1f8ba8",
    size: 1_159_030_304,
};

const QWEN_8B_MMPROJ_Q8: ModelFile = ModelFile {
    file_name: "Qwen3-VL-Embedding-8B.mmproj-Q8_0.gguf",
    url: "https://huggingface.co/mradermacher/Qwen3-VL-Embedding-8B-GGUF/resolve/main/Qwen3-VL-Embedding-8B.mmproj-Q8_0.gguf",
    sha256: "36c44edfbe487f743128e369e53966f2e51e1641e604064f68a443c864b5bb9c",
    size: 752_290_208,
};

/// The f16 file is 15 GB, so the 8B ladder starts at Q8_0.
fn qwen_8b_set(q: Quality) -> ModelSet {
    let (model, mmproj) = match q {
        Quality::Heavy => (
            ModelFile {
                file_name: "Qwen3-VL-Embedding-8B.Q8_0.gguf",
                url: "https://huggingface.co/mradermacher/Qwen3-VL-Embedding-8B-GGUF/resolve/main/Qwen3-VL-Embedding-8B.Q8_0.gguf",
                sha256: "c77299abab613f121ff918f17d085704952b21e986c73a71ec6cdc8a6e43e34b",
                size: 8_048_295_168,
            },
            QWEN_8B_MMPROJ_F16,
        ),
        Quality::Normal => (
            ModelFile {
                file_name: "Qwen3-VL-Embedding-8B.Q6_K.gguf",
                url: "https://huggingface.co/mradermacher/Qwen3-VL-Embedding-8B-GGUF/resolve/main/Qwen3-VL-Embedding-8B.Q6_K.gguf",
                sha256: "c619330e39cad3b87be5a8216fcc9b19801362cda3d38e36f60dbc4f38b1046b",
                size: 6_215_396_608,
            },
            QWEN_8B_MMPROJ_Q8,
        ),
        Quality::Light => (
            ModelFile {
                file_name: "Qwen3-VL-Embedding-8B.Q4_K_M.gguf",
                url: "https://huggingface.co/mradermacher/Qwen3-VL-Embedding-8B-GGUF/resolve/main/Qwen3-VL-Embedding-8B.Q4_K_M.gguf",
                sha256: "a81ec5d9211f03a51369ab1e858f750eabc84a41bec19e129f3d4a0659faf1c7",
                size: 4_677_725_440,
            },
            QWEN_8B_MMPROJ_Q8,
        ),
    };
    ModelSet { model_id: Model::Qwen3Vl8b, quality: q, model, mmproj }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_consistent() {
        let mut names = std::collections::HashMap::new();
        for m in Model::ALL {
            assert_eq!(Model::parse(m.as_str()), Some(m));
            let cal = m.calibration();
            assert!(cal.score_floor < cal.score_ceiling && cal.relative_cut > 0.0);
            for q in Quality::ALL {
                let set = model_set(m, q);
                assert_eq!((set.model_id, set.quality), (m, q));
                for f in [set.model, set.mmproj] {
                    assert!(f.url.starts_with("https://huggingface.co/") && f.url.ends_with(f.file_name), "{}", f.url);
                    assert_eq!(f.sha256.len(), 64);
                    // all models share one directory: a file name must always mean the same file
                    assert_eq!(*names.entry(f.file_name).or_insert(f.sha256), f.sha256, "{}", f.file_name);
                }
            }
        }
    }
}
