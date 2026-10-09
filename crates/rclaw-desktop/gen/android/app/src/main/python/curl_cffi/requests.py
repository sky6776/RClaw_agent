"""Stub: strip impersonate kwargs and use standard requests TLS."""
from requests import Session as _RSession

class Session(_RSession):
    def __init__(self, *a, **k):
        super().__init__(*a, **{x: y for x, y in k.items() if x != "impersonate"})
    def request(self, method, url, **k):
        return super().request(method, url, **{x: y for x, y in k.items() if x != "impersonate"})
    def get(self, url, **k):
        return self.request("GET", url, **k)
    def post(self, url, **k):
        return self.request("POST", url, **k)

def get(url, **k):
    return Session().get(url, **k)

def post(url, **k):
    return Session().post(url, **k)
