use crate::{
    adapters::{Episode, EpisodeOrder},
    projection,
};
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use trakkin_mappings_language::{Record, parse_record};

#[derive(Debug)]
pub(super) struct Alignment {
    pub record: Record,
    pub matches: Vec<EpisodeMatch>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct EpisodeMatch {
    pub left_index: usize,
    pub right_index: usize,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct EpisodeSignature {
    title: String,
    air_date: chrono::NaiveDate,
}

struct EpisodeSpan {
    season_start: u64,
    season_end: u64,
    episode_start: u64,
    episode_end: u64,
}

pub(super) fn align(left: &mut EpisodeOrder, right: &mut EpisodeOrder) -> Result<Vec<Alignment>> {
    for order in [&mut *left, &mut *right] {
        ensure!(
            order.episodes.len() <= 100_000,
            "structural order exceeds 100000 episodes"
        );
        order
            .episodes
            .sort_by_key(|episode| (episode.season, episode.episode));
        let mut coordinates = BTreeSet::new();
        let mut references = BTreeSet::new();
        for episode in &order.episodes {
            ensure!(
                coordinates.insert((episode.season, episode.episode)),
                "default order has ambiguous episode coordinates"
            );
            ensure!(
                references.insert(&episode.reference),
                "default order repeats an episode"
            );
        }
    }
    let references: BTreeMap<_, _> = right
        .episodes
        .iter()
        .enumerate()
        .map(|(index, episode)| (episode.reference.as_str(), index))
        .collect();
    let mut left_metadata = BTreeMap::<_, Vec<_>>::new();
    let mut right_metadata = BTreeMap::<_, Vec<_>>::new();
    for (episodes, metadata) in [
        (&left.episodes, &mut left_metadata),
        (&right.episodes, &mut right_metadata),
    ] {
        for (index, episode) in episodes.iter().enumerate() {
            if let Some(key) = metadata_key(episode) {
                metadata.entry(key).or_default().push(index);
            }
        }
    }
    let mut matches = Vec::new();
    let mut uses = BTreeMap::<usize, usize>::new();
    for (index, episode) in left.episodes.iter().enumerate() {
        let target = if let Some(reference) = &episode.linked_reference {
            references.get(reference.as_str()).copied()
        } else {
            metadata_key(episode).and_then(|key| {
                let left = left_metadata.get(&key)?;
                let right = right_metadata.get(&key)?;
                (left.len() == 1 && right.len() == 1).then(|| right[0])
            })
        };
        if let Some(target) = target {
            matches.push(EpisodeMatch {
                left_index: index,
                right_index: target,
            });
            *uses.entry(target).or_default() += 1;
        }
    }
    let mut runs: Vec<Vec<EpisodeMatch>> = Vec::new();
    for matched in matches
        .into_iter()
        .filter(|matched| uses[&matched.right_index] == 1)
    {
        let continues = runs
            .last()
            .and_then(|run| run.last())
            .is_some_and(|previous| {
                matched.left_index == previous.left_index + 1
                    && matched.right_index == previous.right_index + 1
                    && adjacent(
                        &left.episodes[previous.left_index],
                        &left.episodes[matched.left_index],
                    )
                    && adjacent(
                        &right.episodes[previous.right_index],
                        &right.episodes[matched.right_index],
                    )
            });
        if continues {
            runs.last_mut().unwrap().push(matched);
        } else {
            runs.push(vec![matched]);
        }
    }
    runs.into_iter()
        .map(|matches| {
            let left = expression(left, matches.iter().map(|matched| matched.left_index));
            let right = expression(right, matches.iter().map(|matched| matched.right_index));
            Ok(Alignment {
                record: parse_record(&format!("{left} <=> {right}"))?,
                matches,
            })
        })
        .collect()
}

fn metadata_key(episode: &Episode) -> Option<EpisodeSignature> {
    let title = projection::normalize(episode.title.as_deref()?);
    let air_date =
        chrono::NaiveDate::parse_from_str(episode.air_date.as_deref()?, "%Y-%m-%d").ok()?;
    if title.is_empty() {
        return None;
    }
    Some(EpisodeSignature { title, air_date })
}

fn adjacent(previous: &Episode, next: &Episode) -> bool {
    if previous.season == next.season {
        previous.episode.checked_add(1) == Some(next.episode)
    } else {
        previous.season < next.season
    }
}

fn expression(order: &EpisodeOrder, indices: impl Iterator<Item = usize>) -> String {
    let mut spans: Vec<EpisodeSpan> = Vec::new();
    for index in indices {
        let episode = &order.episodes[index];
        if let Some(span) = spans.last_mut()
            && span.season_end == episode.season
            && span.episode_end.checked_add(1) == Some(episode.episode)
        {
            span.episode_end = episode.episode;
        } else {
            spans.push(EpisodeSpan {
                season_start: episode.season,
                season_end: episode.season,
                episode_start: episode.episode,
                episode_end: episode.episode,
            });
        }
    }
    let mut merged: Vec<EpisodeSpan> = Vec::new();
    for span in spans {
        if let Some(previous) = merged.last_mut()
            && previous.season_end.checked_add(1) == Some(span.season_start)
            && previous.episode_start == span.episode_start
            && previous.episode_end == span.episode_end
        {
            previous.season_end = span.season_end;
        } else {
            merged.push(span);
        }
    }
    let selections = merged
        .into_iter()
        .map(|span| {
            let mut predicates = order
                .context
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>();
            predicates.push(format!(
                "season={}",
                format_range(span.season_start, span.season_end)
            ));
            predicates.push(format!(
                "episode={}",
                format_range(span.episode_start, span.episode_end)
            ));
            format!("{} :: {}", order.reference, predicates.join(","))
        })
        .collect::<Vec<_>>();
    if selections.len() == 1 {
        selections[0].clone()
    } else {
        format!("[{}]", selections.join(","))
    }
}

fn format_range(first: u64, last: u64) -> String {
    if first == last {
        first.to_string()
    } else {
        format!("{first}..{last}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{Catalog, Entity, Unit};
    use trakkin_mappings_language::evaluate;

    fn episode(id: u64, season: u64, ordinal: u64) -> Episode {
        Episode {
            reference: format!("test://episode/{id}"),
            season,
            episode: ordinal,
            title: None,
            air_date: None,
            linked_reference: None,
            observations: Vec::new(),
        }
    }

    fn order(reference: &str, episodes: Vec<Episode>) -> EpisodeOrder {
        EpisodeOrder {
            reference: reference.into(),
            context: BTreeMap::new(),
            observations: Vec::new(),
            episodes,
        }
    }

    fn assert_alignment(left: &mut EpisodeOrder, right: &mut EpisodeOrder, expected: &[&str]) {
        let alignments = align(left, right).unwrap();
        let actual = alignments
            .iter()
            .map(|alignment| alignment.record.statement.claim_id())
            .collect::<BTreeSet<_>>();
        let expected = expected
            .iter()
            .map(|text| parse_record(text).unwrap().statement.claim_id())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        let catalog = Catalog {
            entities: [left, right]
                .into_iter()
                .map(|order| {
                    (
                        order.reference.clone(),
                        Entity {
                            levels: vec!["season".into(), "episode".into()],
                            contexts: order.context.clone(),
                            hierarchy_complete: true,
                            units: order
                                .episodes
                                .iter()
                                .map(|episode| Unit {
                                    reference: episode.reference.clone(),
                                    level: "episode".into(),
                                    attributes: order
                                        .context
                                        .iter()
                                        .map(|(key, value)| (key.clone(), value.clone()))
                                        .chain([
                                            ("season".into(), episode.season.to_string()),
                                            ("episode".into(), episode.episode.to_string()),
                                        ])
                                        .collect(),
                                    coordinate: format!("{}/{}", episode.season, episode.episode),
                                })
                                .collect(),
                            ..Entity::default()
                        },
                    )
                })
                .collect(),
        }
        .finish();
        for alignment in alignments {
            evaluate(&alignment.record.statement, &catalog).unwrap();
        }
    }

    #[test]
    fn season_boundaries_form_closed_ranges_and_ordered_composites() {
        let mut left = order(
            "test://show/1",
            (1..=4).map(|id| episode(id, 1, id)).collect(),
        );
        let mut right = order(
            "test://show/2",
            vec![
                episode(1, 1, 1),
                episode(2, 1, 2),
                episode(3, 2, 1),
                episode(4, 2, 2),
            ],
        );
        for episode in &mut left.episodes {
            episode.linked_reference = Some(episode.reference.clone());
        }
        assert_alignment(
            &mut left,
            &mut right,
            &[
                "test://show/1 :: season=1,episode=1..4 <=> test://show/2 :: season=1..2,episode=1..2",
            ],
        );
        right.episodes[2].season = 1;
        right.episodes[2].episode = 3;
        right.episodes[3].episode = 1;
        assert_alignment(
            &mut left,
            &mut right,
            &[
                "test://show/1 :: season=1,episode=1..4 <=> [test://show/2 :: season=1,episode=1..3,test://show/2 :: season=2,episode=1]",
            ],
        );
    }

    #[test]
    fn coordinates_and_counts_without_identity_evidence_do_not_match() {
        let mut left = order("test://show/1", vec![episode(1, 1, 1), episode(2, 1, 2)]);
        let mut right = order("test://show/2", vec![episode(3, 1, 1), episode(4, 1, 2)]);
        assert_alignment(&mut left, &mut right, &[]);
    }

    #[test]
    fn unique_title_and_date_matches_ignore_numbering_but_not_explicit_conflicts() {
        let mut left = order("test://show/1", vec![episode(1, 1, 1)]);
        let mut right = order("test://show/2", vec![episode(2, 2, 8)]);
        left.episodes[0].title = Some("The Beginning!".into());
        right.episodes[0].title = Some("THE BEGINNING".into());
        left.episodes[0].air_date = Some("2020-01-01".into());
        right.episodes[0].air_date = Some("2020-01-01".into());
        assert_alignment(
            &mut left,
            &mut right,
            &["test://show/1 :: season=1,episode=1 <=> test://show/2 :: season=2,episode=8"],
        );
        left.episodes[0].linked_reference = Some("test://episode/999".into());
        assert_alignment(&mut left, &mut right, &[]);
        left.episodes[0].linked_reference = None;
        right.episodes[0].air_date = Some("2020-01-02".into());
        assert_alignment(&mut left, &mut right, &[]);
        for invalid in ["0000-00-00", "2020-02-30"] {
            left.episodes[0].air_date = Some(invalid.into());
            right.episodes[0].air_date = Some(invalid.into());
            assert_alignment(&mut left, &mut right, &[]);
        }
    }

    #[test]
    fn duplicate_evidence_is_not_arbitrarily_paired() {
        let mut left = order("test://show/1", vec![episode(1, 1, 1), episode(2, 1, 2)]);
        let mut right = order("test://show/2", vec![episode(3, 1, 1)]);
        for episode in left.episodes.iter_mut().chain(&mut right.episodes) {
            episode.title = Some("Arrival".into());
            episode.air_date = Some("2020-01-01".into());
        }
        assert_alignment(&mut left, &mut right, &[]);
        for episode in &mut left.episodes {
            episode.linked_reference = Some(right.episodes[0].reference.clone());
        }
        assert_alignment(&mut left, &mut right, &[]);
    }

    #[test]
    fn unmatched_episodes_and_reorders_break_ranges() {
        let mut left = order(
            "test://show/1",
            (1..=3).map(|id| episode(id, 1, id)).collect(),
        );
        let mut right = order(
            "test://show/2",
            (1..=3).map(|id| episode(id + 10, 1, id)).collect(),
        );
        left.episodes[0].linked_reference = Some(right.episodes[0].reference.clone());
        left.episodes[2].linked_reference = Some(right.episodes[2].reference.clone());
        assert_alignment(
            &mut left,
            &mut right,
            &[
                "test://show/1 :: season=1,episode=1 <=> test://show/2 :: season=1,episode=1",
                "test://show/1 :: season=1,episode=3 <=> test://show/2 :: season=1,episode=3",
            ],
        );
        left.episodes[1].linked_reference = Some(right.episodes[1].reference.clone());
        right.episodes[0].episode = 3;
        right.episodes[2].episode = 1;
        assert_alignment(
            &mut left,
            &mut right,
            &[
                "test://show/1 :: season=1,episode=1 <=> test://show/2 :: season=1,episode=3",
                "test://show/1 :: season=1,episode=2 <=> test://show/2 :: season=1,episode=2",
                "test://show/1 :: season=1,episode=3 <=> test://show/2 :: season=1,episode=1",
            ],
        );
    }

    #[test]
    fn duplicate_native_coordinates_are_diagnosed() {
        let mut left = order("test://show/1", vec![episode(1, 1, 1), episode(2, 1, 1)]);
        let mut right = order("test://show/2", vec![]);
        assert!(
            align(&mut left, &mut right)
                .unwrap_err()
                .to_string()
                .contains("ambiguous episode coordinates")
        );
    }
}
