#!/usr/bin/env python3
"""Quick Tavily web search script.

Usage:
    python3 tavily_search.py "your search query"
    TAVILY_API_KEY=tvly-xxx python3 tavily_search.py "your search query"
"""
import os
import sys
import json


def main():
    api_key = os.environ.get("TAVILY_API_KEY")
    if not api_key:
        print("Error: TAVILY_API_KEY not set.", file=sys.stderr)
        print("Get a free key at https://app.tavily.com and set:", file=sys.stderr)
        print("  export TAVILY_API_KEY=tvly-...", file=sys.stderr)
        sys.exit(1)

    if len(sys.argv) < 2:
        print("Usage: tavily_search.py <query>", file=sys.stderr)
        sys.exit(1)

    query = " ".join(sys.argv[1:])

    from tavily import TavilyClient
    client = TavilyClient(api_key=api_key)
    result = client.search(query=query, max_results=5)

    # Print results in a readable format
    print(f"Query: {query}\n")
    for i, r in enumerate(result.get("results", []), 1):
        print(f"--- Result {i} ---")
        print(f"Title:   {r.get('title', 'N/A')}")
        print(f"URL:     {r.get('url', 'N/A')}")
        print(f"Score:   {r.get('score', 0):.4f}")
        content = r.get("content", "")
        # Truncate long content
        if len(content) > 500:
            content = content[:500] + "..."
        print(f"Content: {content}")
        print()

    if "answer" in result:
        print(f"Answer: {result['answer']}")


if __name__ == "__main__":
    main()
