import { createClawhipOmxClient } from './clawhip-sdk.mjs';

const clientPromise = createClawhipOmxClient();

export async function onHookEvent(event, sdk) {
  const client = await clientPromise;
  const sessionState = await sdk.omx.session.read();

  const result = await client.emitFromHookEvent(event, {
    context: {
      agent_name: 'omx',
      ...(sessionState?.session_id && !event?.session_id ? { session_id: sessionState.session_id } : {}),
      ...(sessionState?.cwd && !event?.context?.worktree_path ? { worktree_path: sessionState.cwd } : {}),
    },
  });

  if (result?.skipped) {
    await sdk.log.info('clawhip OMX hook skipped non-contract event', {
      event: event?.event,
      normalized_event: event?.context?.normalized_event ?? null,
      reason: result.reason,
    });
  }

  return result;
}
