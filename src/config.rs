use envconfig::Envconfig;

#[derive(Envconfig)]
pub struct Config {
    pub redis_url: String,
    pub instance_name: String,
    pub irc_server: String,
    pub irc_nickname: String,
}
