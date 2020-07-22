# Spectacles Proxy

The Spectacles proxy is responsible for handling all of the data interactions with Discord.

- [x] Ratelimited REST calls to Discord
- [ ] Caching REST responses
- [ ] Caching ingested data from the [gateway](https://github.com/spec-tacles/gateway)

## Usage

The proxy communicates with other services using the [Spectacles spec](https://github.com/spec-tacles/spec) over AMQP.

- JS: [`brokers.js`](https://github.com/spec-tacles/brokers.js)
- C#: [`Spectacles.NET`](https://github.com/spec-tacles/Spectacles.NET)
- Go: [`spec-tacles/go`](https://github.com/spec-tacles/go)
- Rust: [`rustacles`](https://github.com/spec-tacles/rustacles)

The proxy can be configured with the following options. This file must be called `proxy.toml` and exist in the CWD. Alternatively, specify the values in the environment variables named adjacently. The following example contains the default values.

```toml
[redis]
url = "redis://localhost:6379" # REDIS_URL

[amqp]
url = "amqp://localhost:5672/%2f" # AMQP_URL
group = "rest" # AMQP_GROUP
# subgroup = "foo" # AMQP_SUBGROUP
event = "REQUEST" # AMQP_EVENT
```

Requests can be made by publishing on the specified event to the specified group. The data must be serialized in JSON format.

```json
{
	"method": "GET",
	"path": "/users/1234",
	"query": {
		"foo": "bar"
	},
	"body": {
		"abc": "xyz"
	},
	"headers": {
		"def": "uvw"
	}
}
```

`query`, `body`, and `headers` are optional. Body must be any valid JSON value.

The response is returned on the callback queue in the following JSON format.

```json
{
	"status": 200,
	"headers": {
		"foo": "bar"
	},
	"url": "https://discord.com/api/v6/users/4567",
	"body": "a"
}
```

`url` represents the full, final URL of the request. `body` is any valid JSON as returned from the server.