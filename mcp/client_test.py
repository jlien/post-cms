"""Drive mcp/server.py with the real MCP client library (not raw stdio frames)."""
import asyncio
import sys

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


async def main():
    params = StdioServerParameters(command="python3", args=["mcp/server.py"])
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            init = await session.initialize()
            print("1) initialize ->", init.serverInfo.name, "protocol", init.protocolVersion)

            tools = await session.list_tools()
            print("2) tools/list ->", sorted(t.name for t in tools.tools))

            # Draft before
            before = await session.call_tool("list_posts", {"status": "draft"})
            print("3) list_posts(draft) before ->", before.content[0].text[:200])

            # Create a draft through the real client
            created = await session.call_tool(
                "create_draft",
                {
                    "slug": "mcp-client-test",
                    "title": "MCP Client Test",
                    "body": "Written by the real MCP Python client.",
                    "tags": ["test"],
                    "description": "verifies the stdio MCP server",
                },
            )
            print("4) create_draft ->", created.content[0].text[:200])

            after = await session.call_tool("list_posts", {"status": "draft"})
            assert "mcp-client-test" in after.content[0].text, "draft missing after create!"
            print("5) list_posts(draft) after -> mcp-client-test present ✓")

            got = await session.call_tool("get_post", {"slug": "mcp-client-test"})
            print("6) get_post ->", got.content[0].text[:150])

            build = await session.call_tool("build_site", {})
            print("7) build_site ->", build.content[0].text[:150])


asyncio.run(main())
