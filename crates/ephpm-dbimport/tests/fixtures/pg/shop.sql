--
-- PostgreSQL database dump
--

SET statement_timeout = 0;
SET client_encoding = 'UTF8';
SELECT pg_catalog.set_config('search_path', '', false);

CREATE EXTENSION IF NOT EXISTS "uuid-ossp" WITH SCHEMA public;

CREATE SEQUENCE public.customers_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

CREATE TABLE public.customers (
    id integer NOT NULL,
    name text NOT NULL,
    email text
);

ALTER TABLE public.customers OWNER TO shop;

--
-- Data for Name: customers; Type: TABLE DATA; Schema: public
--

COPY public.customers (id, name, email) FROM stdin;
1	Ada Lovelace	ada@example.test
2	Alan Turing	\N
3	Grace Hopper	grace@example.test
\.

COMMENT ON TABLE public.customers IS 'people who buy things';

GRANT ALL ON TABLE public.customers TO shop;

--
-- PostgreSQL database dump complete
--
