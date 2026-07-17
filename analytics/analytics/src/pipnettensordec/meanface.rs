// Copyright (C) 2026 Collabora Ltd
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Meanface tables and neighbor reverse-index computation for PIPNet.
//!
//! The meanface arrays are vendored from the original PIPNet implementation
//! (<https://github.com/jhb86253817/PIPNet>, MIT license) via the
//! `yakhyo/pipnet-onnx` port
//! (<https://github.com/yakhyo/pipnet-onnx/blob/main/model/meanface.py>).
//! They describe the average (normalized) landmark positions for each
//! supported schema and are used, together with `NUM_NB`, to precompute
//! which neighboring landmarks contribute a prediction for each landmark
//! ("reverse indices").

/// Number of neighbors considered for each landmark, as used by the original
/// PIPNet training configuration and by `yakhyo/pipnet-onnx`.
pub const NUM_NB: usize = 10;

/// 300W(+CelebA) layout: 68 landmarks, 136 floats (x0,y0,x1,y1,...).
#[rustfmt::skip]
#[allow(clippy::excessive_precision)] // Vendored upstream values, kept at full precision.
static MEANFACE_300W_68: [f32; 136] = [
    0.05558998895410058, 0.23848280098218655, 0.05894856684324656, 0.3590187767402909,
    0.0736574254414371, 0.4792196439871159, 0.09980016420365162, 0.5959029676167197,
    0.14678670154995865, 0.7035615597409001, 0.21847188218752928, 0.7971705893013413,
    0.30554692814599393, 0.8750572978073209, 0.4018434142644611, 0.9365018059444535,
    0.5100536090382116, 0.9521295666029498, 0.6162039414413925, 0.9309467340899419,
    0.7094522484942942, 0.8669275031738761, 0.7940993502957612, 0.7879369615524398,
    0.8627063649669019, 0.6933756633633967, 0.9072386130534111, 0.5836975017700834,
    0.9298874997796132, 0.4657004930314701, 0.9405202670724796, 0.346063993805527,
    0.9425419553088846, 0.22558131891345742, 0.13304298285530403, 0.14853071838028062,
    0.18873587368440375, 0.09596491613770254, 0.2673231915839219, 0.08084218279128136,
    0.34878638553224905, 0.09253591849498964, 0.4226713753717798, 0.12466063383809506,
    0.5618513152452376, 0.11839668911898667, 0.6394952560845826, 0.08480191391770678,
    0.7204375851516752, 0.07249669092117161, 0.7988615904537885, 0.08766933146893043,
    0.8534884939460948, 0.1380096813348583, 0.49610677423740546, 0.21516740699375395,
    0.49709661403980665, 0.2928875699060973, 0.4982292618461611, 0.3699985379939941,
    0.49982965173254235, 0.4494119144493957, 0.406772397599095, 0.5032397294041786,
    0.45231994786363067, 0.5197953144002292, 0.49969685987914064, 0.5332489262413073,
    0.5470074224053442, 0.518413595827126, 0.5892261151542287, 0.5023530079850803,
    0.22414578747180394, 0.22835847349949062, 0.27262947128194215, 0.19915251892241678,
    0.3306759252861797, 0.20026034220607236, 0.38044435864341913, 0.23839196034290633,
    0.32884072789429913, 0.24902443794896897, 0.2707409300714473, 0.24950886025380967,
    0.6086826011068529, 0.23465048639345917, 0.660397116846103, 0.1937087938594717,
    0.7177815187666494, 0.19317079039835858, 0.7652328176062365, 0.22088822845258235,
    0.722727677909097, 0.24195514178450958, 0.6658378927310327, 0.2441554205021945,
    0.32894370935769124, 0.6496589505331646, 0.39347179739100613, 0.6216899667490776,
    0.4571976492475472, 0.60794251109236, 0.4990484623797022, 0.6190124015360254,
    0.5465555522325872, 0.6071477960565326, 0.6116127327356168, 0.6205387097430033,
    0.6742318496058836, 0.6437466364395467, 0.6144773141699744, 0.7077526646009754,
    0.5526442055374252, 0.7363350735898412, 0.5018120662554302, 0.7424476622366345,
    0.4554458875556401, 0.7382303858617719, 0.3923750731597415, 0.7118887028663435,
    0.35530766372404593, 0.6524479416354049, 0.457111071610868, 0.6467108367268608,
    0.49974082228815025, 0.6508406774477011, 0.5477027224368399, 0.6451242819422733,
    0.6478392760505715, 0.647852382880368, 0.5488474760115958, 0.6779061893042735,
    0.5001073351044452, 0.6845280260362221, 0.4564831746654594, 0.6799300301441035,
];

/// WFLW layout: 98 landmarks, 196 floats (x0,y0,x1,y1,...).
#[rustfmt::skip]
#[allow(clippy::excessive_precision)] // Vendored upstream values, kept at full precision.
static MEANFACE_WFLW_98: [f32; 196] = [
    0.07960419395480703, 0.3921576875344978, 0.08315055593117261, 0.43509551571809146,
    0.08675705281580391, 0.47810288286566444, 0.09141892980469117, 0.5210356946467262,
    0.09839925903528965, 0.5637522280060038, 0.10871037524559955, 0.6060410614977951,
    0.12314562992759207, 0.6475338700558225, 0.14242389255404694, 0.6877152027028081,
    0.16706295456951875, 0.7259564546408682, 0.19693946055282413, 0.761730578566735,
    0.23131827931527224, 0.7948205670466106, 0.2691730934906831, 0.825332081636482,
    0.3099415030959131, 0.853325959406618, 0.3535202097901413, 0.8782538906229107,
    0.40089023799272033, 0.8984102434399625, 0.4529251732310723, 0.9112191359814178,
    0.5078640056794708, 0.9146712690731943, 0.5616519666079889, 0.9094327772020283,
    0.6119216923689698, 0.8950540037623425, 0.6574617882337107, 0.8738084866764846,
    0.6994820494908942, 0.8482660530943744, 0.7388135339780575, 0.8198750461527688,
    0.775158750479601, 0.788989141243473, 0.8078785221990765, 0.7555462713420953,
    0.8361052138935441, 0.7195542055115057, 0.8592123871172533, 0.6812759034843933,
    0.8771159986952748, 0.6412243940605555, 0.8902481006481506, 0.5999743595282084,
    0.8992952868651163, 0.5580032282594118, 0.9050110573289222, 0.5156548913779377,
    0.908338439928252, 0.4731336721500472, 0.9104896075281127, 0.4305382486815422,
    0.9124796341441906, 0.38798192678294363, 0.18465941635742913, 0.35063191749632183,
    0.24110421889338157, 0.31190394310826886, 0.3003235400132397, 0.30828189837331976,
    0.3603094923651325, 0.3135606490643205, 0.4171060234289877, 0.32433417646045615,
    0.416842139562573, 0.3526729965541497, 0.36011177591813404, 0.3439660526998693,
    0.3000863121140166, 0.33890077494044946, 0.24116055928407834, 0.34065620413845005,
    0.5709736930161899, 0.321407825750195, 0.6305694459247149, 0.30972642336729495,
    0.6895161625920927, 0.3036453838462943, 0.7488591859761683, 0.3069143844433495,
    0.8030471337135181, 0.3435156012309415, 0.7485083446528741, 0.3348759588212388,
    0.6893025057931884, 0.33403402013776456, 0.6304822892126991, 0.34038458762875695,
    0.5710009285609654, 0.34988479902594455, 0.4954171902473609, 0.40202330022004634,
    0.49604903449415433, 0.4592869389138444, 0.49644391662771625, 0.5162862508677217,
    0.4981161256057368, 0.5703284628419502, 0.40749001573145566, 0.5983629921847019,
    0.4537396729649631, 0.6057169923583451, 0.5007345777827058, 0.6116695615531077,
    0.5448481727980428, 0.6044131443745976, 0.5882140504891681, 0.5961738788380111,
    0.24303324896316683, 0.40721003719912746, 0.27771706732644313, 0.3907171413930685,
    0.31847706697401107, 0.38417234007271117, 0.3621792860449715, 0.3900847721320633,
    0.3965299162804086, 0.41071434661355205, 0.3586805562211872, 0.4203724421417311,
    0.31847860588240934, 0.4237674602252073, 0.2789458001651631, 0.41942757306509065,
    0.5938514626567266, 0.4090628827047304, 0.6303565516542536, 0.3864501652756091,
    0.6774844732813035, 0.3809319896905685, 0.7150854850525555, 0.3875173254527522,
    0.747519807465081, 0.4025187328459307, 0.7155172856447009, 0.4145958479293519,
    0.680051949453018, 0.420041513473271, 0.6359056750107122, 0.41803782782566573,
    0.33916483987223056, 0.6968581311227738, 0.40008790639758807, 0.6758101185779204,
    0.47181947887764153, 0.6678850445191217, 0.5025394453374782, 0.6682917934792593,
    0.5337748367911458, 0.6671949030019636, 0.6015915330083903, 0.6742535357237751,
    0.6587068892667173, 0.6932163943648724, 0.6192795131720007, 0.7283129162844936,
    0.5665923267827963, 0.7550248076404299, 0.5031303335863617, 0.7648348885181623,
    0.4371030429958871, 0.7572539606688756, 0.3814909500115824, 0.7320595346122074,
    0.35129809553480984, 0.6986839074746692, 0.4247987356100664, 0.69127609583798,
    0.5027677238758598, 0.6911145821740593, 0.576997542122097, 0.6896269708051024,
    0.6471352843446794, 0.6948977432227927, 0.5799932528781817, 0.7185288017567538,
    0.5024914756021335, 0.7285408331555782, 0.4218115644247556, 0.7209126133193829,
    0.3219750495122499, 0.40376441481225156, 0.6751136343101699, 0.40023415216110797,
];

/// A named group of skeleton edges, declared per-variant as data instead of
/// being spelled out per-branch in a `match`.
#[derive(Debug, Clone, Copy)]
pub enum EdgeGroup {
    /// Consecutive chain of landmarks `start..=end`.
    Chain(u32, u32),
    /// Like [`EdgeGroup::Chain`], plus a closing edge from `end` back to `start`.
    Loop(u32, u32),
}

impl EdgeGroup {
    fn edges(self) -> Vec<(u32, u32)> {
        match self {
            Self::Chain(start, end) => chain_edges(start, end),
            Self::Loop(start, end) => loop_edges(start, end),
        }
    }
}

/// All data describing one supported PIPNet landmark schema.
///
/// To support a new variant (e.g. AFLW-19), add one more entry to [`VARIANTS`]
/// with its meanface table and skeleton edge groups; no other code needs to
/// change.
pub struct VariantInfo {
    /// Semantic tag attached to the emitted keypoints group.
    pub semantic_tag: &'static str,
    meanface: &'static [f32],
    edge_groups: &'static [EdgeGroup],
}

impl VariantInfo {
    /// Number of landmarks (`NUM_LMS`), derived from the meanface table's flat `x,y` pairs.
    pub fn num_lms(&self) -> usize {
        self.meanface.len() / 2
    }

    /// Computes the neighbor reverse-index table for this variant. Cheap enough to compute
    /// once per `set_caps` call and cache on the element instance rather than lazily
    /// memoizing here.
    pub fn reverse_index(&self) -> ReverseIndex {
        build_neighbor_indices(self.meanface, NUM_NB)
    }

    /// Computes the skeleton connectivity pairs (landmark index pairs) used for overlay
    /// rendering, flattened as `[a0, b0, a1, b1, ...]`. Cheap enough to compute once per
    /// `set_caps` call and cache on the element instance.
    pub fn skeleton_pairs(&self) -> Vec<i32> {
        self.edge_groups
            .iter()
            .flat_map(|group| group.edges())
            .flat_map(|(a, b)| [a as i32, b as i32])
            .collect()
    }
}

/// All supported PIPNet landmark schemas. To add a new variant, append an
/// entry here; every consumer (caps template, variant lookup, decoding)
/// iterates or searches this table instead of matching on a fixed enum.
pub static VARIANTS: &[VariantInfo] = &[
    VariantInfo {
        semantic_tag: "face-300W-68",
        meanface: &MEANFACE_300W_68,
        edge_groups: &[
            EdgeGroup::Chain(0, 16),  // jaw
            EdgeGroup::Chain(17, 21), // right eyebrow
            EdgeGroup::Chain(22, 26), // left eyebrow
            EdgeGroup::Chain(27, 30), // nose bridge
            EdgeGroup::Chain(31, 35), // nose bottom
            EdgeGroup::Loop(36, 41),  // right eye
            EdgeGroup::Loop(42, 47),  // left eye
            EdgeGroup::Loop(48, 59),  // outer mouth
            EdgeGroup::Loop(60, 67),  // inner mouth
        ],
    },
    VariantInfo {
        semantic_tag: "face-WFLW-98",
        meanface: &MEANFACE_WFLW_98,
        edge_groups: &[
            EdgeGroup::Chain(0, 32),  // contour
            EdgeGroup::Chain(33, 41), // right eyebrow
            EdgeGroup::Chain(42, 50), // left eyebrow
            EdgeGroup::Chain(51, 54), // nose bridge
            EdgeGroup::Chain(55, 59), // nose bottom
            EdgeGroup::Loop(60, 67),  // right eye
            EdgeGroup::Loop(68, 75),  // left eye
            EdgeGroup::Loop(76, 87),  // outer mouth
            EdgeGroup::Loop(88, 95),  // inner mouth
                                      // 96, 97 are the pupils: isolated points, no edges.
        ],
    },
];

/// Guesses the variant from the number of landmarks (`NUM_LMS`).
pub fn find_variant(num_lms: usize) -> Option<&'static VariantInfo> {
    VARIANTS.iter().find(|v| v.num_lms() == num_lms)
}

/// Edges connecting consecutive landmarks `start..=end`.
fn chain_edges(start: u32, end: u32) -> Vec<(u32, u32)> {
    (start..end).map(|i| (i, i + 1)).collect()
}

/// Edges connecting consecutive landmarks `start..=end`, plus a closing edge
/// from `end` back to `start`.
fn loop_edges(start: u32, end: u32) -> Vec<(u32, u32)> {
    let mut edges = chain_edges(start, end);
    edges.push((end, start));
    edges
}

/// Reverse-index table mapping, for each landmark, the `(landmark, neighbor
/// slot)` pairs of neighbors that emit a prediction about it. Padded by
/// repetition so each landmark has exactly `max_len` entries, matching
/// `_build_neighbor_indices` in `yakhyo/pipnet-onnx`.
#[derive(Debug)]
pub struct ReverseIndex {
    pub index1: Vec<u32>,
    pub index2: Vec<u32>,
    pub max_len: usize,
}

/// Computes the `NUM_NB` nearest neighbors of each landmark in `meanface`
/// (flattened `x,y` pairs), then builds the reverse mapping: for each
/// landmark, which `(landmark, neighbor slot)` pairs predict it.
fn build_neighbor_indices(meanface: &[f32], num_nb: usize) -> ReverseIndex {
    let points = meanface.as_chunks::<2>().0;
    let num_lms = points.len();

    // For each landmark, indices of its `num_nb` nearest neighbors (excluding
    // itself), sorted by increasing squared distance.
    let mut neighbor_indices = Vec::with_capacity(num_lms);
    for [px, py] in points.iter() {
        let mut dists: Vec<(usize, f32)> = points
            .iter()
            .enumerate()
            .map(|(j, [x, y])| (j, (x - px).powi(2) + (y - py).powi(2)))
            .collect();
        dists.sort_by(|a, b| a.1.total_cmp(&b.1));
        // Skip index 0, which is the landmark itself (distance 0).
        let nearest: Vec<u32> = dists[1..=num_nb].iter().map(|&(j, _)| j as u32).collect();
        neighbor_indices.push(nearest);
    }

    // Reverse map: for each landmark `neighbor`, collect `(i, j)` such that
    // `neighbor_indices[i][j] == neighbor`.
    let mut reversed: Vec<(Vec<u32>, Vec<u32>)> = vec![(Vec::new(), Vec::new()); num_lms];
    for (i, neighbors) in neighbor_indices.iter().enumerate() {
        for (j, &neighbor) in neighbors.iter().enumerate() {
            let neighbor = neighbor as usize;
            reversed[neighbor].0.push(i as u32);
            reversed[neighbor].1.push(j as u32);
        }
    }

    let max_len = reversed
        .iter()
        .map(|(idx1, _)| idx1.len())
        .max()
        .unwrap_or(0);

    let mut index1 = Vec::with_capacity(num_lms * max_len);
    let mut index2 = Vec::with_capacity(num_lms * max_len);
    for (idx1, idx2) in &reversed {
        for k in 0..max_len {
            // Pad by repeating entries (wrapping) so every landmark has the
            // same neighbor count, matching the upstream Python behavior.
            let src = k % idx1.len().max(1);
            index1.push(idx1[src]);
            index2.push(idx2[src]);
        }
    }

    ReverseIndex {
        index1,
        index2,
        max_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_from_num_lms() {
        assert_eq!(find_variant(68).map(|v| v.num_lms()), Some(68));
        assert_eq!(find_variant(98).map(|v| v.num_lms()), Some(98));
        assert_eq!(find_variant(19).map(|v| v.num_lms()), None);
    }

    #[test]
    fn reverse_index_shape_68() {
        let variant = find_variant(68).unwrap();
        let ri = variant.reverse_index();
        assert!(ri.max_len > 0);
        assert_eq!(ri.index1.len(), variant.num_lms() * ri.max_len);
        assert_eq!(ri.index2.len(), variant.num_lms() * ri.max_len);
        assert!(ri.index1.iter().all(|&i| (i as usize) < variant.num_lms()));
        assert!(ri.index2.iter().all(|&j| (j as usize) < NUM_NB));
    }

    #[test]
    fn reverse_index_shape_98() {
        let variant = find_variant(98).unwrap();
        let ri = variant.reverse_index();
        assert!(ri.max_len > 0);
        assert_eq!(ri.index1.len(), variant.num_lms() * ri.max_len);
        assert_eq!(ri.index2.len(), variant.num_lms() * ri.max_len);
        assert!(ri.index1.iter().all(|&i| (i as usize) < variant.num_lms()));
        assert!(ri.index2.iter().all(|&j| (j as usize) < NUM_NB));
    }

    #[test]
    fn skeleton_pairs_68_within_bounds() {
        let pairs = find_variant(68).unwrap().skeleton_pairs();
        assert!(!pairs.is_empty());
        assert_eq!(pairs.len() % 2, 0);
        assert!(pairs.iter().all(|&i| (0..68).contains(&i)));
    }

    #[test]
    fn skeleton_pairs_98_within_bounds() {
        let pairs = find_variant(98).unwrap().skeleton_pairs();
        assert!(!pairs.is_empty());
        assert_eq!(pairs.len() % 2, 0);
        assert!(pairs.iter().all(|&i| (0..98).contains(&i)));
    }

    /// Exercises every entry in `VARIANTS` generically, so that adding a new
    /// variant to the table is automatically covered without writing a new
    /// per-variant test.
    #[test]
    fn all_variants_reverse_index_and_skeleton_pairs_within_bounds() {
        for variant in VARIANTS {
            let num_lms = variant.num_lms();
            let ri = variant.reverse_index();
            assert!(ri.max_len > 0);
            assert_eq!(ri.index1.len(), num_lms * ri.max_len);
            assert_eq!(ri.index2.len(), num_lms * ri.max_len);
            assert!(ri.index1.iter().all(|&i| (i as usize) < num_lms));
            assert!(ri.index2.iter().all(|&j| (j as usize) < NUM_NB));

            let pairs = variant.skeleton_pairs();
            assert!(!pairs.is_empty());
            assert_eq!(pairs.len() % 2, 0);
            let num_lms = num_lms as i32;
            assert!(pairs.iter().all(|&i| (0..num_lms).contains(&i)));
        }
    }
}
