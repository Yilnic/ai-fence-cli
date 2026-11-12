"""Desktop bootstrap/auth-owner contract regressions. No real credentials or IO."""
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

source=Path(__file__).resolve().parents[1]/'assets/codex_desktop_auth.py'
spec=importlib.util.spec_from_file_location('auth_under_test',source)
auth=importlib.util.module_from_spec(spec);spec.loader.exec_module(auth)

class Owner:
    def __init__(self):self.calls=[];self.fail=False
    def call(self,method,params):
        self.calls.append((method,params))
        if self.fail:raise auth.AuthError('Native credential owner disconnected during authentication')
        if method=='getAuthStatus':
            return {'authMethod':'chatgpt','requiresOpenaiAuth':True,
                    **({'authToken':'unit-test-access-value'} if params.get('includeToken') else {})}
        if method=='account/read':return {'account':{'type':'chatgpt'},'requiresOpenaiAuth':True}
        return {}
    def close(self):pass

class DesktopAuthCompatibility(unittest.TestCase):
    def setUp(self):
        self.owner=Owner();self.native=[];self.client=[]
        def send(value):
            self.native.append(value)
            self.bridge.runtime({'id':value['id'],'result':{}})
        self.bridge=auth.Bridge({},send,self.client.append,owner_factory=lambda *_:self.owner)
        self.tokens=patch.object(auth,'access_tokens',return_value={
            'accessToken':'unit-test-access-value','chatgptAccountId':'unit-test-account'})
        self.tokens.start()
    def tearDown(self):
        self.bridge.close();self.tokens.stop()
    def test_desktop_bootstrap_get_auth_status(self):
        request={'id':'desktop-auth:unit','method':'getAuthStatus',
                 'params':{'includeToken':False,'refreshToken':False}}
        self.assertTrue(self.bridge.client(request))
        self.assertEqual(self.owner.calls, [('getAuthStatus',request['params'])])
        result=self.client[-1]['result']
        self.assertEqual(result['authMethod'],'chatgpt')
        self.assertNotIn('authToken',result)
        self.assertEqual(self.native[0]['method'],'account/login/start')
        self.assertEqual(self.native[0]['params']['type'],'chatgptAuthTokens')
    def test_explicit_access_token_request_uses_owner(self):
        self.assertTrue(self.bridge.client({'id':2,'method':'getAuthStatus',
            'params':{'includeToken':True,'refreshToken':False}}))
        self.assertEqual(self.client[-1]['result']['authToken'],'unit-test-access-value')
        self.assertNotIn('refreshToken',self.client[-1]['result'])
    def test_missing_owner_login_does_not_fabricate_authentication(self):
        self.owner.fail=True
        self.assertTrue(self.bridge.client({'id':3,'method':'getAuthStatus','params':{}}))
        self.assertEqual(self.client[-1]['error']['code'],-32001)
        self.assertNotIn('result',self.client[-1])
    def test_account_read_still_uses_owner(self):
        self.assertTrue(self.bridge.client({'id':4,'method':'account/read','params':{'refreshToken':False}}))
        self.assertEqual(self.client[-1]['result']['account']['type'],'chatgpt')
    def test_unrelated_calls_are_not_intercepted(self):
        self.assertFalse(self.bridge.client({'id':5,'method':'thread/read','params':{'threadId':'unit'}}))
        self.assertEqual(self.owner.calls,[])
    def test_notifications_do_not_create_auth_operations(self):
        self.assertFalse(self.bridge.client({'method':'getAuthStatus','params':{}}))
        self.assertEqual(self.owner.calls,[])

    def test_refresh_parameters_are_forwarded_unchanged(self):
        for method in ('getAuthStatus', 'account/read'):
            with self.subTest(method=method):
                params = {'includeToken': False, 'refreshToken': True}
                self.assertTrue(self.bridge.client(dict(id=7, method=method, params=params)))
                self.assertEqual(self.owner.calls[-1], (method, params))

    def test_bootstrap_order_synchronizes_runtime_only_once(self):
        self.assertFalse(self.bridge.client(dict(id=1, method='initialize')))
        self.assertTrue(self.bridge.client(dict(id=2, method='getAuthStatus',
                                               params=dict(includeToken=False, refreshToken=False))))
        self.assertFalse(self.bridge.client(dict(method='initialized')))
        self.assertTrue(self.bridge.client(dict(id=3, method='account/read',
                                               params=dict(refreshToken=False))))
        self.assertEqual([method for method, _ in self.owner.calls], ['getAuthStatus', 'account/read'])
        self.assertEqual([value['method'] for value in self.native], ['account/login/start'])

    def test_logout_clears_runtime_and_cached_account(self):
        self.bridge.client(dict(id=1, method='getAuthStatus'))
        self.bridge.client(dict(id=2, method='account/logout'))
        self.assertEqual(self.owner.calls[-1], ('account/logout', {}))
        self.assertEqual(self.native[-1]['method'], 'account/logout')
        self.assertIsNone(self.bridge.current)
        self.assertIn('result', self.client[-1])

    def test_account_change_resynchronizes_external_tokens(self):
        self.bridge.client(dict(id=1, method='getAuthStatus'))
        changed = dict(accessToken='different-unit-value', chatgptAccountId='different-unit-account')
        with patch.object(auth, 'access_tokens', return_value=changed):
            self.bridge.client(dict(id=2, method='account/read'))
        self.assertEqual(self.native[-1]['params'], dict(type='chatgptAuthTokens', **changed))
        self.assertEqual(self.bridge.current, changed)
        self.assertEqual(len(self.native), 2)

    def test_owner_token_removal_logs_out_runtime(self):
        self.bridge.client(dict(id=1, method='getAuthStatus'))
        with patch.object(auth, 'access_tokens', return_value=None):
            self.bridge.client(dict(id=2, method='account/read'))
        self.assertEqual(self.native[-1]['method'], 'account/logout')
        self.assertIsNone(self.bridge.current)

    def test_runtime_login_failure_remains_an_error(self):
        def reject(value):
            self.bridge.runtime(dict(id=value['id'], error=dict(code=-32602)))
        self.bridge.native_send = reject
        self.bridge.client(dict(id=1, method='getAuthStatus'))
        self.assertIn('error', self.client[-1])
        self.assertNotIn('result', self.client[-1])
        self.assertIsNone(self.bridge.current)

if __name__=='__main__':unittest.main()
