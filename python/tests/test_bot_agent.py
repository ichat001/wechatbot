"""Tests for bot_agent sanitization."""

from wechatbot.protocol import DEFAULT_BOT_AGENT, sanitize_bot_agent


def test_empty_input_falls_back_to_default():
    assert sanitize_bot_agent(None) == DEFAULT_BOT_AGENT
    assert sanitize_bot_agent("") == DEFAULT_BOT_AGENT
    assert sanitize_bot_agent("   ") == DEFAULT_BOT_AGENT


def test_single_product():
    assert sanitize_bot_agent("MyApp/1.2") == "MyApp/1.2"


def test_product_with_comment():
    assert sanitize_bot_agent("MyApp/1.2 (prod build)") == "MyApp/1.2 (prod build)"


def test_multiple_products():
    assert sanitize_bot_agent("MyApp/1.2 (prod) Lib/0.3") == "MyApp/1.2 (prod) Lib/0.3"


def test_normalizes_whitespace():
    assert sanitize_bot_agent("  MyApp/1.2   Lib/0.3 ") == "MyApp/1.2 Lib/0.3"


def test_invalid_input_falls_back_wholesale():
    assert sanitize_bot_agent("no-slash") == DEFAULT_BOT_AGENT
    assert sanitize_bot_agent("bad name/1.0 !!!") == DEFAULT_BOT_AGENT
    assert sanitize_bot_agent("(orphan comment)") == DEFAULT_BOT_AGENT
    assert sanitize_bot_agent("App/1.0 (unclosed") == DEFAULT_BOT_AGENT
    assert sanitize_bot_agent("App/1.0 (nested (comment))") == DEFAULT_BOT_AGENT


def test_overlong_tokens_rejected():
    assert sanitize_bot_agent("a" * 33 + "/1.0") == DEFAULT_BOT_AGENT
    assert sanitize_bot_agent("App/" + "1" * 33) == DEFAULT_BOT_AGENT


def test_over_byte_cap_rejected():
    assert sanitize_bot_agent(("App/1.0 " * 40).strip()) == DEFAULT_BOT_AGENT
