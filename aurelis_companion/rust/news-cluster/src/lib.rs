//! Native headline-clustering engine for Aurelis's `core/domain` `NewsClusterAggregator`.
//!
//! Ports `cluster`/`levenshtein` (`NewsClusterAggregator.kt`) line-for-line: greedy
//! single-pass clustering where each headline joins the first existing cluster
//! containing a title within [`ENGINE_VERSION`]-stable Levenshtein distance, else
//! starts a new cluster. Owns no long-lived state: one call receives the whole
//! headline list and returns the clustering.
//!
//! `NewsClusterAggregator.cluster` is not currently called from anywhere in the app
//! (verified via a full-repo search) — this engine is equally dormant, kept ready
//! for whichever Kotlin implementation ends up wired in.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use serde::{Deserialize, Serialize};

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlineIn {
    /// Lowercased title -- `levenshtein` in Kotlin lowercases both sides at the call
    /// site, so the crate takes the already-lowercased string as a caller primitive.
    pub title: String,
    pub published_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterInput {
    pub headlines: Vec<HeadlineIn>,
    #[serde(default = "default_distance_threshold")]
    pub distance_threshold: i32,
}

fn default_distance_threshold() -> i32 {
    15
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterOut {
    /// Indices into the input `headlines` list, in original encounter order.
    pub member_indices: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterOutput {
    pub clusters: Vec<ClusterOut>,
}

/// Byte-for-byte port of `levenshtein` (NewsClusterAggregator.kt): two-row optimization.
fn levenshtein(lhs: &str, rhs: &str) -> usize {
    let a: Vec<char> = lhs.chars().collect();
    let b: Vec<char> = rhs.chars().collect();
    let (m, n) = (a.len(), b.len());

    let mut cost: Vec<usize> = (0..=m).collect();
    let mut new_cost = vec![0usize; m + 1];

    for i in 1..=n {
        new_cost[0] = i;
        for j in 1..=m {
            let match_cost = if a[j - 1] == b[i - 1] { 0 } else { 1 };
            let replace = cost[j - 1] + match_cost;
            let insert = cost[j] + 1;
            let delete = new_cost[j - 1] + 1;
            new_cost[j] = insert.min(delete).min(replace);
        }
        cost.copy_from_slice(&new_cost);
    }
    cost[m]
}

/// Byte-for-byte port of `cluster` (NewsClusterAggregator.kt).
pub fn cluster(input: &ClusterInput) -> Vec<ClusterOut> {
    let mut clusters: Vec<Vec<usize>> = Vec::new();

    for (idx, headline) in input.headlines.iter().enumerate() {
        let mut added = false;
        for cluster in clusters.iter_mut() {
            let is_similar = cluster.iter().any(|&member_idx| {
                levenshtein(&headline.title, &input.headlines[member_idx].title)
                    <= input.distance_threshold as usize
            });
            if is_similar {
                cluster.push(idx);
                added = true;
                break;
            }
        }
        if !added {
            clusters.push(vec![idx]);
        }
    }

    let mut out: Vec<ClusterOut> = clusters
        .into_iter()
        .map(|member_indices| ClusterOut { member_indices })
        .collect();

    out.sort_by(|a, b| {
        let size_cmp = b.member_indices.len().cmp(&a.member_indices.len());
        if size_cmp != std::cmp::Ordering::Equal {
            return size_cmp;
        }
        let max_a = a
            .member_indices
            .iter()
            .map(|&i| input.headlines[i].published_at_ms)
            .max()
            .unwrap_or(0);
        let max_b = b
            .member_indices
            .iter()
            .map(|&i| input.headlines[i].published_at_ms)
            .max()
            .unwrap_or(0);
        max_b.cmp(&max_a)
    });
    out
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_intelligence_news_NewsClusterNativeBridge_nativeCluster<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<ClusterInput>(&raw) {
            Ok(input) => ClusterOutput { clusters: cluster(&input) },
            Err(_) => ClusterOutput { clusters: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"clusters\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_intelligence_news_NewsClusterNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(title: &str, published_at_ms: i64) -> HeadlineIn {
        HeadlineIn {
            title: title.to_lowercase(),
            published_at_ms,
        }
    }

    #[test]
    fn similar_titles_join_the_same_cluster() {
        let input = ClusterInput {
            headlines: vec![
                h("PM announces new budget plan", 100),
                h("PM announces new budget plans", 200),
                h("Local team wins championship", 300),
            ],
            distance_threshold: 15,
        };
        let clusters = cluster(&input);
        assert_eq!(clusters.len(), 2);
        let budget_cluster = clusters.iter().find(|c| c.member_indices.contains(&0)).unwrap();
        assert!(budget_cluster.member_indices.contains(&1));
    }

    #[test]
    fn dissimilar_titles_stay_in_separate_clusters() {
        let input = ClusterInput {
            headlines: vec![
                h("Stock markets rally on rate cut hopes", 100),
                h("Volcano erupts in remote pacific island", 200),
            ],
            distance_threshold: 15,
        };
        let clusters = cluster(&input);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn clusters_sorted_by_size_then_recency() {
        let input = ClusterInput {
            headlines: vec![
                h("Prime minister announces new economic budget plan today", 100),
                h("Volcano erupts in remote pacific island chain overnight", 50),
                h("Prime minister announces new economic budget plan update", 500),
                h("Prime minister announces new economic budget plan finalised", 900),
            ],
            distance_threshold: 15,
        };
        let clusters = cluster(&input);
        // "Prime minister ..." cluster (3 members) must sort before the lone volcano story.
        assert_eq!(clusters[0].member_indices.len(), 3);
        assert_eq!(clusters[1].member_indices.len(), 1);
    }

    #[test]
    fn empty_input_returns_no_clusters() {
        let input = ClusterInput {
            headlines: vec![],
            distance_threshold: 15,
        };
        assert!(cluster(&input).is_empty());
    }

    #[test]
    fn levenshtein_distance_basic_cases() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("same title", "same title"), 0);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
