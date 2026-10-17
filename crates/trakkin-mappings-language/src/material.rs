use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
};

use num_traits::{CheckedAdd, CheckedDiv, CheckedMul, CheckedSub};
use serde::Serialize;

use crate::{
    Correspondence, CorrespondenceCapture, CorrespondenceLimits, CorrespondenceRelation,
    MaterialAlignment, RationalPosition, UnitSpan,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MaterialRule {
    pub antecedent: BTreeSet<u32>,
    pub consequent: BTreeSet<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MaterialGraph {
    capture: CorrespondenceCapture,
    semantic_revision: String,
    units: BTreeMap<String, BTreeSet<u32>>,
    atom_count: usize,
    groups: Vec<MaterialRule>,
    implications: Vec<MaterialRule>,
}

impl MaterialGraph {
    pub fn capture(&self) -> &CorrespondenceCapture {
        &self.capture
    }
    pub fn semantic_revision(&self) -> &str {
        &self.semantic_revision
    }
    pub fn unit(&self, reference: &str) -> Option<&BTreeSet<u32>> {
        self.units.get(reference)
    }
    pub fn atom_count(&self) -> usize {
        self.atom_count
    }
    pub fn groups(&self) -> &[MaterialRule] {
        &self.groups
    }
    pub fn implications(&self) -> &[MaterialRule] {
        &self.implications
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterialGraphError {
    InvalidCapture,
    LimitExceeded,
    Conflicting { reference: String },
}

impl fmt::Display for MaterialGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapture => formatter.write_str("material graph dependencies do not match"),
            Self::LimitExceeded => {
                formatter.write_str("material graph exceeded its arithmetic or work limits")
            }
            Self::Conflicting { reference } => write!(
                formatter,
                "conflicting material correspondence for {reference}"
            ),
        }
    }
}

impl std::error::Error for MaterialGraphError {}

struct Budget(usize);

impl Budget {
    fn spend(&mut self, work: usize) -> Result<(), MaterialGraphError> {
        self.0 = self
            .0
            .checked_sub(work)
            .ok_or(MaterialGraphError::LimitExceeded)?;
        Ok(())
    }
}

struct Components {
    parents: Vec<usize>,
    sizes: Vec<usize>,
}

impl Components {
    fn new(count: usize) -> Self {
        Self {
            parents: (0..count).collect(),
            sizes: vec![1; count],
        }
    }

    fn root(&mut self, mut index: usize) -> usize {
        while self.parents[index] != index {
            self.parents[index] = self.parents[self.parents[index]];
            index = self.parents[index];
        }
        index
    }

    fn join(&mut self, left: usize, right: usize) {
        let mut left = self.root(left);
        let mut right = self.root(right);
        if left == right {
            return;
        }
        if self.sizes[left] < self.sizes[right] {
            std::mem::swap(&mut left, &mut right);
        }
        self.parents[right] = left;
        self.sizes[left] += self.sizes[right];
    }
}

type Cuts = BTreeMap<String, BTreeSet<RationalPosition>>;
type Pieces = BTreeMap<String, BTreeMap<RationalPosition, (RationalPosition, usize)>>;

fn whole(reference: &str) -> UnitSpan {
    UnitSpan {
        reference: reference.to_owned(),
        start: RationalPosition::from_integer(0),
        end: RationalPosition::from_integer(1),
    }
}

fn register(
    cuts: &mut Cuts,
    reference: &str,
    capture: &CorrespondenceCapture,
    budget: &mut Budget,
) -> Result<(), MaterialGraphError> {
    budget.spend(reference.len().max(1))?;
    let Some((source, opaque)) = reference.split_once("://") else {
        return Err(MaterialGraphError::InvalidCapture);
    };
    if opaque.is_empty()
        || reference.len() > 4096
        || !capture.reference_revisions.contains_key(source)
    {
        return Err(MaterialGraphError::InvalidCapture);
    }
    cuts.entry(reference.to_owned()).or_insert_with(|| {
        [
            RationalPosition::from_integer(0),
            RationalPosition::from_integer(1),
        ]
        .into()
    });
    Ok(())
}

fn translate(
    position: &RationalPosition,
    from: &UnitSpan,
    to: &UnitSpan,
) -> Result<RationalPosition, MaterialGraphError> {
    let from_extent = from
        .end
        .checked_sub(&from.start)
        .ok_or(MaterialGraphError::LimitExceeded)?;
    let to_extent = to
        .end
        .checked_sub(&to.start)
        .ok_or(MaterialGraphError::LimitExceeded)?;
    position
        .checked_sub(&from.start)
        .and_then(|offset| offset.checked_mul(&to_extent))
        .and_then(|offset| offset.checked_div(&from_extent))
        .and_then(|offset| offset.checked_add(&to.start))
        .ok_or(MaterialGraphError::LimitExceeded)
}

fn propagate(
    cuts: &mut Cuts,
    edges: &[MaterialAlignment],
    limits: CorrespondenceLimits,
    budget: &mut Budget,
) -> Result<(), MaterialGraphError> {
    let mut neighbors: BTreeMap<&str, Vec<(&UnitSpan, &UnitSpan)>> = BTreeMap::new();
    for edge in edges {
        neighbors
            .entry(&edge.left.reference)
            .or_default()
            .push((&edge.left, &edge.right));
        neighbors
            .entry(&edge.right.reference)
            .or_default()
            .push((&edge.right, &edge.left));
    }
    let mut queue = VecDeque::new();
    for (reference, positions) in cuts.iter() {
        for position in positions {
            budget.spend(reference.len().max(1))?;
            queue.push_back((reference.clone(), *position));
        }
    }
    let mut pieces = queue.len().saturating_sub(cuts.len());
    if pieces > limits.maximum_members {
        return Err(MaterialGraphError::LimitExceeded);
    }
    while let Some((reference, position)) = queue.pop_front() {
        for (from, to) in neighbors.get(reference.as_str()).into_iter().flatten() {
            budget.spend(1)?;
            if position < from.start || position > from.end {
                continue;
            }
            let mapped = translate(&position, from, to)?;
            if cuts
                .get_mut(&to.reference)
                .expect("registered reference")
                .insert(mapped)
            {
                pieces += 1;
                if pieces > limits.maximum_members {
                    return Err(MaterialGraphError::LimitExceeded);
                }
                budget.spend(to.reference.len().max(1))?;
                queue.push_back((to.reference.clone(), mapped));
            }
        }
    }
    Ok(())
}

fn span_atoms(
    span: &UnitSpan,
    pieces: &Pieces,
    components: &mut Components,
    ids: &BTreeMap<usize, u32>,
    budget: &mut Budget,
) -> Result<BTreeSet<u32>, MaterialGraphError> {
    let mut atoms = BTreeSet::new();
    for (_, (end, index)) in pieces[&span.reference].range(span.start..span.end) {
        budget.spend(1)?;
        if *end <= span.end {
            atoms.insert(ids[&components.root(*index)]);
        }
    }
    Ok(atoms)
}

pub fn build_material_graph(
    references: &BTreeSet<String>,
    correspondences: &[Correspondence],
    capture: CorrespondenceCapture,
    limits: CorrespondenceLimits,
) -> Result<MaterialGraph, MaterialGraphError> {
    if references.len() > limits.maximum_members || correspondences.len() > limits.maximum_members {
        return Err(MaterialGraphError::LimitExceeded);
    }
    if capture.mapping_revision.is_empty()
        || capture.mapping_revision.len() > 4096
        || capture.mapping_revision.chars().any(char::is_control)
    {
        return Err(MaterialGraphError::InvalidCapture);
    }
    let mut budget = Budget(limits.maximum_work);
    budget.spend(capture.mapping_revision.len())?;
    for (source, revision) in &capture.reference_revisions {
        budget.spend(source.len().saturating_add(revision.len()))?;
        if source.is_empty()
            || revision.is_empty()
            || revision.len() > 4096
            || revision.chars().any(char::is_control)
        {
            return Err(MaterialGraphError::InvalidCapture);
        }
    }
    let mut cuts = Cuts::new();
    for reference in references {
        register(&mut cuts, reference, &capture, &mut budget)?;
    }
    let mut edges = Vec::new();
    let mut implications = Vec::new();
    let mut groups = Vec::new();
    for correspondence in correspondences {
        budget.spend(correspondence.capture().mapping_revision.len().max(1))?;
        for (source, revision) in &correspondence.capture().reference_revisions {
            budget.spend(source.len().saturating_add(revision.len()).max(1))?;
        }
        if correspondence.capture().mapping_revision != capture.mapping_revision
            || correspondence
                .capture()
                .reference_revisions
                .iter()
                .any(|(source, revision)| capture.reference_revisions.get(source) != Some(revision))
        {
            return Err(MaterialGraphError::InvalidCapture);
        }
        match correspondence.relation() {
            CorrespondenceRelation::Exact(pairs) => {
                for pair in pairs {
                    budget.spend(pair.left.len().saturating_add(pair.right.len()))?;
                    edges.push(MaterialAlignment {
                        left: whole(&pair.left),
                        right: whole(&pair.right),
                    });
                }
            }
            CorrespondenceRelation::OrderedCoverage(segments)
            | CorrespondenceRelation::PositiveImplication(segments) => {
                let destination = if matches!(
                    correspondence.relation(),
                    CorrespondenceRelation::PositiveImplication(_)
                ) {
                    &mut implications
                } else {
                    &mut edges
                };
                for segment in segments {
                    budget.spend(
                        segment
                            .left
                            .reference
                            .len()
                            .saturating_add(segment.right.reference.len()),
                    )?;
                    destination.push(segment.clone());
                }
            }
            CorrespondenceRelation::UnorderedCoverage { left, right } => {
                for reference in left.iter().chain(right) {
                    register(&mut cuts, reference, &capture, &mut budget)?;
                }
                groups.push((left, right));
            }
        }
        for span in correspondence
            .left_tail()
            .unmapped
            .iter()
            .chain(&correspondence.right_tail().unmapped)
        {
            register(&mut cuts, &span.reference, &capture, &mut budget)?;
        }
    }
    for edge in edges.iter().chain(&implications) {
        for span in [&edge.left, &edge.right] {
            register(&mut cuts, &span.reference, &capture, &mut budget)?;
            cuts.get_mut(&span.reference)
                .expect("registered reference")
                .extend([span.start, span.end]);
        }
    }
    propagate(&mut cuts, &edges, limits, &mut budget)?;
    let mut pieces = Pieces::new();
    let mut count = 0;
    for (reference, positions) in &cuts {
        let positions: Vec<_> = positions.iter().copied().collect();
        let mut fragments = BTreeMap::new();
        for pair in positions.windows(2) {
            budget.spend(1)?;
            fragments.insert(pair[0], (pair[1], count));
            count += 1;
        }
        pieces.insert(reference.clone(), fragments);
    }
    let mut components = Components::new(count);
    for edge in &edges {
        for (start, (end, left)) in
            pieces[&edge.left.reference].range(edge.left.start..edge.left.end)
        {
            budget.spend(1)?;
            let mapped_start = translate(start, &edge.left, &edge.right)?;
            let mapped_end = translate(end, &edge.left, &edge.right)?;
            let (right_end, right) = pieces[&edge.right.reference]
                .get(&mapped_start)
                .ok_or_else(|| MaterialGraphError::Conflicting {
                    reference: edge.right.reference.clone(),
                })?;
            if *right_end != mapped_end {
                return Err(MaterialGraphError::Conflicting {
                    reference: edge.right.reference.clone(),
                });
            }
            components.join(*left, *right);
        }
    }
    let mut ids = BTreeMap::new();
    let mut units = BTreeMap::new();
    for (reference, fragments) in &pieces {
        let mut atoms = BTreeSet::new();
        for (_, index) in fragments.values() {
            budget.spend(1)?;
            let root = components.root(*index);
            let next = u32::try_from(ids.len()).map_err(|_| MaterialGraphError::LimitExceeded)?;
            let id = *ids.entry(root).or_insert(next);
            if !atoms.insert(id) {
                return Err(MaterialGraphError::Conflicting {
                    reference: reference.clone(),
                });
            }
        }
        units.insert(reference.clone(), atoms);
    }
    let mut group_rules = BTreeSet::new();
    for (left, right) in groups {
        let mut collect = |references: &[String]| -> Result<BTreeSet<u32>, MaterialGraphError> {
            let mut result = BTreeSet::new();
            for reference in references {
                budget.spend(units[reference].len())?;
                result.extend(&units[reference]);
            }
            Ok(result)
        };
        let mut left = collect(left)?;
        let mut right = collect(right)?;
        if left > right {
            std::mem::swap(&mut left, &mut right);
        }
        group_rules.insert(MaterialRule {
            antecedent: left,
            consequent: right,
        });
    }
    let mut implication_rules = BTreeSet::new();
    for edge in implications {
        implication_rules.insert(MaterialRule {
            antecedent: span_atoms(&edge.left, &pieces, &mut components, &ids, &mut budget)?,
            consequent: span_atoms(&edge.right, &pieces, &mut components, &ids, &mut budget)?,
        });
    }
    Ok(MaterialGraph {
        capture,
        semantic_revision: crate::semantic_revision(),
        atom_count: ids.len(),
        units,
        groups: group_rules.into_iter().collect(),
        implications: implication_rules.into_iter().collect(),
    })
}
