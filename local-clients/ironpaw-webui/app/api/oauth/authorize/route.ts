import { authorizationUrl } from '../../../../server/oauth/google';

interface RequestBody {
  provider?: string;
  redirect_uri?: string;
  state?: string;
  code_challenge?: string;
  login_hint?: string;
  scopes?: string[];
}

export async function POST(request: Request) {
  try {
    const body = await request.json() as RequestBody;
    if (body.provider !== 'google'
      || !body.redirect_uri
      || !body.state
      || !body.code_challenge
      || !body.login_hint
      || !Array.isArray(body.scopes)) {
      return new Response('invalid OAuth authorization request', { status: 400 });
    }
    return Response.json({
      authorization_url: authorizationUrl({
        redirectUri: body.redirect_uri,
        state: body.state,
        codeChallenge: body.code_challenge,
        loginHint: body.login_hint,
        scopes: body.scopes,
      }),
    });
  } catch (error) {
    console.error(error);
    return new Response('could not start OAuth authorization', { status: 400 });
  }
}

