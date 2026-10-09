"""Termux stub for curl_cffi: no Android wheel; delegate to standard requests."""
from .requests import Session, get, post  # noqa: F401
