import { sealGrant } from '../../../../server/oauth/grant';
import { exchangeCode } from '../../../../server/oauth/google';

interface RequestBody {
  provider?: string;
  code?: string;
  code_verifier?: string;
  redirect_uri?: string;
  binding_key?: string;
}

export async function POST(request: Request) {
  try {
    const body = await request.json() as RequestBody;
    if (body.provider !== 'google'
      || !body.code
      || !body.code_verifier
      || !body.redirect_uri
      || !body.binding_key
      || Buffer.from(body.binding_key, 'base64url').length !== 32) {
      return new Response('invalid OAuth exchange request', { status: 400 });
    }
    const refreshToken = await exchangeCode({
      code: body.code,
      codeVerifier: body.code_verifier,
      redirectUri: body.redirect_uri,
    });
    return Response.json({
      grant: sealGrant({
        provider: body.provider,
        refreshToken,
        bindingKey: body.binding_key,
      }),
    });
  } catch (error) {
    console.error(error);
    return new Response('could not exchange OAuth code', { status: 502 });
  }
}

