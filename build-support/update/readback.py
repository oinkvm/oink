"""Verify canonical Pages bytes across an asynchronous snapshot transition."""
import json
import time
import urllib.error
import urllib.parse
import feed

# Pages advertises max-age=600. Allow one cache lifetime plus a propagation
# margin, only for an authenticated transition between verified snapshots.
# A cold response must match a verified snapshot; no HTTP/signature check is relaxed.
BUDGET_SECONDS = 630
INTERVAL_SECONDS = 5
HEADERS = ('Age', 'Cache-Control', 'X-Cache', 'X-Cache-Hits', 'X-Served-By',
           'ETag', 'Last-Modified', 'Date', 'X-Fastly-Request-ID')


def log(value):
    print(json.dumps(value, sort_keys=True), flush=True)


def fetch(url, limit, path, attempt, role, expected):
    headers = {}
    try:
        raw = feed.download(url, limit, False, True, headers)
        status = 200
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        headers.update(error.headers or {})
        error.close()
        raw, status = None, 404
    log({'event': 'public_readback', 'path': path, 'url': url, 'attempt': attempt,
         'role': role, 'status': status, 'expected_sha256': None if expected is None else feed.digest(expected),
         'observed_sha256': None if raw is None else feed.digest(raw),
         'headers': {key: {k.lower(): v for k, v in headers.items()}.get(key.lower()) for key in HEADERS}})
    return raw


def verify(expected, previous, error='deployment_digest'):
    """A cache-busted probe diagnoses visibility; it can NEVER accept a deploy.

    Only a complete round of exact canonical responses (including required
    absence) succeeds. Unknown origin bytes fail immediately. Known previous
    bytes or a canonical cache contradicted by an exact cold target response
    may converge within the Pages cache lifetime, for deploy AND rollback.
    """
    start = time.monotonic()
    attempt = 0
    while True:
        attempt += 1
        stale = []
        for path, raw in expected.items():
            if time.monotonic() - start > BUDGET_SECONDS:
                raise ValueError(error)
            url = feed.BASE + path
            old = previous.get(path)
            limit = max(feed.MAX_FEED, len(raw or b''), len(old or b''))
            actual = fetch(url, limit, path, attempt, 'canonical', raw)
            if actual == raw:
                continue
            stale.append(path)
            identity = 'absent' if raw is None else feed.digest(raw)
            query = urllib.parse.urlencode({'oink-readback': identity + '-' + str(attempt)})
            fresh = fetch(url + '?' + query, limit, path, attempt, 'uncached-diagnostic', raw)
            known_previous = path in previous and actual == old
            # A prior failed deployment can leave a different cached candidate.
            # Only an exact cold target response authenticates this as cache lag;
            # it still cannot stand in for canonical client bytes.
            feed.require(fresh in (raw, old) if known_previous else fresh == raw, error)
        if not stale:
            return
        remaining = BUDGET_SECONDS - (time.monotonic() - start)
        feed.require(remaining > 0, error)
        log({'event': 'public_readback_pending', 'attempt': attempt,
             'transitional_paths': stale, 'elapsed_seconds': round(time.monotonic() - start, 3),
             'budget_seconds': BUDGET_SECONDS})
        time.sleep(min(INTERVAL_SECONDS, remaining))
