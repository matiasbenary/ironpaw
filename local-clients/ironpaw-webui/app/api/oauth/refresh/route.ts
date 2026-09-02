import { openGrant, verifyProof } from '../../../../server/oauth/grant';
import { refreshAccessToken } from '../../../../server/oauth/google';

interface RequestBody {
  provider?: string;
  grant?: string;
  proof?: string;
}

export async function POST(request: Request) {
  try {
    const body = await request.json() as RequestBody;
    if (body.provider !== 'google' || !body.grant || !body.proof) {
      return new Response('invalid OAuth refresh request', { status: 400 });
    }
    const grant = openGrant(body.grant);
    if (grant.provider !== body.provider
      || !verifyProof(body.grant, grant.bindingKey, body.proof)) {
      return new Response('invalid OAuth grant', { status: 401 });
    }
    return Response.json({ access_token: await refreshAccessToken(grant.refreshToken) });
  } catch (error) {
    console.error(error);
    return new Response('could not refresh OAuth token', { status: 502 });
  }
}
