-- History replay filters events by time and by kind + time (the latest classification of
-- each domain before T, activity histograms, the feed at T).
CREATE INDEX events_ts ON events(ts);
CREATE INDEX events_kind_ts ON events(kind, ts);
