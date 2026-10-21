// POST /api/waitlist {email} -> 204. One key per address in the WAITLIST KV namespace
// ("windows:<email>" -> {at}), so signing up twice changes nothing. `website` is a honeypot field
// the form hides: a bot that fills it gets the same 204 and nothing is stored.
// ponytail: no rate limit or CAPTCHA; add Turnstile if junk sign-ups show up in KV.
export async function onRequestPost({ request, env }) {
  let body;
  try {
    body = await request.json();
  } catch {
    return error(400, 'Send the form as JSON.');
  }
  if (body?.website) return new Response(null, { status: 204 });
  const email = String(body?.email ?? '').trim().toLowerCase();
  if (email.length > 254 || !/^[^@\s]+@[^@\s.]+(\.[^@\s.]+)+$/.test(email)) return error(400, 'Enter a valid email address.');
  const key = `windows:${email}`;
  if (!(await env.WAITLIST.get(key))) await env.WAITLIST.put(key, JSON.stringify({ at: new Date().toISOString() }));
  return new Response(null, { status: 204 });
}

const error = (status, message) => Response.json({ error: message }, { status });
