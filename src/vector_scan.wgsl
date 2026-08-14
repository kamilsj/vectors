struct Parameters {
    dimensions: u32,
    candidate_count: u32,
    row_count: u32,
    metric: u32,
    indexed: u32,
    query_norm: f32,
    dispatch_width: u32,
    candidate_offset: u32,
}

struct ScanResult {
    score: f32,
    state: u32,
}

@group(0) @binding(0) var<storage, read> vectors: array<f32>;
@group(0) @binding(1) var<storage, read> norms: array<f32>;
@group(0) @binding(2) var<storage, read> present: array<u32>;
@group(0) @binding(3) var<storage, read> candidates: array<u32>;
@group(0) @binding(4) var<storage, read> query: array<f32>;
@group(0) @binding(5) var<uniform> parameters: Parameters;
@group(0) @binding(6) var<storage, read_write> results: array<ScanResult>;

@compute @workgroup_size(128)
fn scan(@builtin(global_invocation_id) invocation: vec3<u32>) {
    let candidate = invocation.x + invocation.y * parameters.dispatch_width;
    if candidate >= parameters.candidate_count {
        return;
    }

    var row = candidate + parameters.candidate_offset;
    if parameters.indexed != 0u {
        row = candidates[candidate];
    }
    if row >= parameters.row_count || present[row] == 0u {
        results[candidate] = ScanResult(0.0, 0u);
        return;
    }

    let base = row * parameters.dimensions;
    var score = 0.0;
    var dimension = 0u;
    loop {
        if dimension >= parameters.dimensions {
            break;
        }
        let stored = vectors[base + dimension];
        let requested = query[dimension];
        if parameters.metric == 0u || parameters.metric == 1u {
            let difference = stored - requested;
            score += difference * difference;
        } else {
            score += stored * requested;
        }
        dimension += 1u;
    }

    if parameters.metric == 0u {
        score = sqrt(score);
    } else if parameters.metric == 2u {
        if norms[row] == 0.0 || parameters.query_norm == 0.0 {
            results[candidate] = ScanResult(0.0, 2u);
            return;
        }
        score = 1.0 - score / (norms[row] * parameters.query_norm);
    } else if parameters.metric == 4u {
        score = -score;
    }
    results[candidate] = ScanResult(score, 1u);
}
