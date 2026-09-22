//! Simple, dependency-free music search: word-AND matching with field-weighted
//! scoring. Good enough for voice ("play bohemian rhapsody queen") without an
//! embedding model.

use crate::library::Library;

pub fn norm(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ranked search results: (track id, score), best first.
pub fn search(lib: &Library, query: &str, limit: usize) -> Vec<(u64, u32)> {
    let q = norm(query);
    if q.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = q.split(' ').filter(|w| !w.is_empty()).collect();
    let mut scored: Vec<(u64, u32)> = Vec::new();
    for t in lib.iter().filter(|t| t.playable) {
        let title = norm(&t.title);
        let artist = norm(&t.artist);
        let album = norm(&t.album);
        let genre = norm(&t.genre);
        let haystack = format!("{title} {artist} {album} {genre}");
        // Every query word must appear somewhere in the track's fields.
        if !words.iter().all(|w| haystack.contains(w)) {
            continue;
        }
        let mut score = 0u32;
        for w in &words {
            if title.contains(w) {
                score += 5;
            }
            if artist.contains(w) {
                score += 4;
            }
            if album.contains(w) {
                score += 3;
            }
            if genre.contains(w) {
                score += 3;
            }
            // Full-title phrase hits are strong.
            if title == q || title.starts_with(&q) {
                score += 8;
            }
        }
        scored.push((t.id, score));
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(limit.max(1));
    scored
}
