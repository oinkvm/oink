"""Pages success is asynchronous; only exact canonical bytes may commit state."""
import sys
from pathlib import Path
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[2]/'build-support/update'))
import feed
import readback

class ReadbackTests(unittest.TestCase):
    def run_case(self, responses, expected=None, previous=None):
        clock=[0.0];seen=[]
        def fetch(url,limit,redirects,revalidate,headers):
            self.assertFalse(redirects);self.assertTrue(revalidate)
            seen.append(url);headers.update({'Cache-Control':'max-age=600','Age':'0','X-Cache':'HIT','ETag':'old'})
            return responses(url,clock[0])
        with patch.object(feed,'download',side_effect=fetch),patch.object(readback.time,'monotonic',side_effect=lambda:clock[0]),patch.object(readback.time,'sleep',side_effect=lambda seconds:clock.__setitem__(0,clock[0]+seconds)),patch.object(readback,'log'):
            readback.verify(expected or {'heartbeat.json':b'new'},previous or {'heartbeat.json':b'old'})
        return clock[0],seen

    def test_cache_busted_success_never_accepts_stale_canonical(self):
        def responses(url,t):return b'new' if '?' in url or t>=10 else b'old'
        elapsed,seen=self.run_case(responses)
        self.assertEqual(elapsed,10)
        self.assertEqual(sum('?' not in u for u in seen),3)
        self.assertTrue(any('?' in u for u in seen))

    def test_warm_http_cache_ignores_no_cache_until_canonical_converges(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import threading
        calls=[]
        class WarmEdge(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_GET(self):
                calls.append((self.path,self.headers.get('Cache-Control')))
                canonical=sum('?' not in p for p,_ in calls)
                self.send_response(200)
                self.send_header('Cache-Control','max-age=600')
                self.send_header('Age','0')
                self.send_header('X-Cache','HIT')
                self.end_headers()
                self.wfile.write(b'new' if '?' in self.path or canonical>1 else b'old')
        server=ThreadingHTTPServer(('127.0.0.1',0),WarmEdge)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            with patch.object(feed,'BASE',f'http://127.0.0.1:{server.server_port}/'),patch.object(readback.time,'sleep'),patch.object(readback,'log') as output:
                readback.verify({'heartbeat.json':b'new'},{'heartbeat.json':b'old'})
            self.assertEqual(sum('?' not in p for p,_ in calls),2)
            self.assertTrue(all(h=='no-cache' for _,h in calls))
            self.assertEqual(output.call_args.args[0]['role'],'canonical')
            self.assertEqual(output.call_args.args[0]['observed_sha256'],feed.digest(b'new'))
        finally:server.shutdown();server.server_close();thread.join()

    def test_orphan_cache_needs_exact_cold_target_and_canonical_convergence(self):
        def responses(url,t):return b'new' if '?' in url or t>=5 else b'orphaned prior deployment'
        elapsed,seen=self.run_case(responses)
        self.assertEqual(elapsed,5)
        self.assertEqual(sum('?' not in u for u in seen),2)

    def test_unknown_bytes_fail_immediately_without_waiting(self):
        with self.assertRaisesRegex(ValueError,'deployment_digest'):
            self.run_case(lambda url,t:b'not either verified snapshot')

    def test_correct_canonical_but_wrong_fresh_probe_is_rejected(self):
        def responses(url,t):return b'unknown' if '?' in url else b'old'
        with self.assertRaisesRegex(ValueError,'deployment_digest'):
            self.run_case(responses)

    def test_permanent_known_old_is_bounded_not_accepted(self):
        with patch.object(readback,'BUDGET_SECONDS',10):
            with self.assertRaisesRegex(ValueError,'deployment_digest'):
                self.run_case(lambda url,t:b'old')

    def test_rollback_waits_for_previous_bytes_not_the_candidate(self):
        def responses(url,t):return b'candidate' if t<5 else b'frozen parent'
        elapsed,seen=self.run_case(responses,{'feed.xml':b'frozen parent'},{'feed.xml':b'candidate'})
        self.assertEqual(elapsed,5)
        self.assertTrue(all('/feed.xml' in u for u in seen))

    def test_every_path_must_match_in_same_round(self):
        def responses(url,t):
            if '/a?' in url:return b'A'
            if '/b?' in url:return b'B'
            if url.endswith('/a'):return b'A' if t>=5 else b'old A'
            return b'B' if t>=10 else b'old B'
        elapsed,_=self.run_case(responses,{'a':b'A','b':b'B'},{'a':b'old A','b':b'old B'})
        self.assertEqual(elapsed,10)

    def test_missing_new_path_can_only_be_known_previous_absence(self):
        import urllib.error
        def responses(url,t):
            if t<5:raise urllib.error.HTTPError(url,404,'Not Found',{},None)
            return b'new'
        elapsed,_=self.run_case(responses,{'new.xml':b'new'},{'new.xml':None})
        self.assertEqual(elapsed,5)

    def test_rollback_requires_disabled_prefix_absent(self):
        import urllib.error
        def responses(url,t):
            if t<5:return b'candidate'
            raise urllib.error.HTTPError(url,404,'Not Found',{},None)
        elapsed,_=self.run_case(responses,{'feed.xml':None},{'feed.xml':b'candidate'})
        self.assertEqual(elapsed,5)

    def test_intel_channels_require_canonical_convergence_in_same_round(self):
        paths = [f'mac/v1/mac-2026-01/x64/{channel}/appcast.xml' for channel in ('stable', 'beta')]
        def responses(url, t):
            if '?' not in url and t < (5 if '/stable/' in url else 10):
                return b'old Intel feed'
            return b'new Intel feed'
        elapsed, seen = self.run_case(responses, dict.fromkeys(paths, b'new Intel feed'),
                                      dict.fromkeys(paths, b'old Intel feed'))
        self.assertEqual(elapsed, 10)
        for path in paths:
            self.assertEqual(seen.count(feed.BASE + path), 3)

    def test_first_intel_publication_can_converge_from_known_absence(self):
        import urllib.error
        path = 'mac/v1/mac-2026-01/x64/stable/appcast.xml'
        def responses(url, t):
            if t < 5:
                raise urllib.error.HTTPError(url, 404, 'Not Found', {}, None)
            return b'new Intel feed'
        elapsed, seen = self.run_case(responses, {path: b'new Intel feed'}, {path: None})
        self.assertEqual(elapsed, 5)
        self.assertEqual(seen.count(feed.BASE + path), 2)

if __name__=='__main__':unittest.main()
