"""Real Python peer for cross-language service integration tests."""
from s5r import HostClient, ServiceKey, Worker

worker = Worker("python-guest", "1")
for key in ("native.echo@1", "worker-b.echo@1"):
    worker.allow_service(ServiceKey.parse(key))


@worker.service(ServiceKey.parse("python-guest.echo@1"))
async def echo(input, context):
    if isinstance(input, dict) and "target" in input:
        return await HostClient.services().invoke(ServiceKey.parse(input["target"]), {})
    return {
        "provider": context.extension_id,
        "caller": context.caller_extension_id,
        "workspace": context.working_dir,
        "session": context.session_id,
    }


worker.run_stdio()
