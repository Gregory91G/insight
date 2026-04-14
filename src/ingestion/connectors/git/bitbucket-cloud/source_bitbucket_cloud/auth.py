"""Bitbucket Cloud authentication helpers."""

import base64


def auth_headers(token: str, username: str = "") -> dict:
    """Build authentication headers for Bitbucket Cloud API requests.

    Bitbucket personal API tokens require Basic Auth (username:token).
    Workspace access tokens use Bearer auth (no username needed).
    """
    if username:
        credentials = base64.b64encode(f"{username}:{token}".encode()).decode()
        auth_value = f"Basic {credentials}"
    else:
        auth_value = f"Bearer {token}"

    return {
        "Authorization": auth_value,
        "Accept": "application/json",
        "User-Agent": "insight-bitbucket-cloud-connector/1.0",
    }
